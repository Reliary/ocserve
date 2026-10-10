//! `/vcs/*` — git status/diff/info, freeze-parity (WEBUI-PLAN W5).
//!
//! Shapes probed live against opencode 1.18.31 (2026-10-08):
//! - `GET /vcs` → `{branch, default_branch}` (strings or null)
//! - `GET /vcs/status` → `FileStatus[]` `{file, additions, deletions, status}`
//!   (`status` ∈ added|deleted|modified), sorted by file
//! - `GET /vcs/diff?mode=git|branch[&context=N]` → `FileDiff[]`
//!   `{file, patch, additions, deletions, status}`
//! - `GET /vcs/diff/raw` → the raw patch text (empty string when clean)
//! - bad `mode` → 400 Effect Query envelope
//!
//! Subprocess discipline (the "large diffs are slow" complaint): a single
//! `status --porcelain` + a single `diff --numstat` for stats, and ONE
//! batched `diff --patch` for all tracked files (`git diff … -- .`), split
//! by `diff --git` headers — never one spawn per file. Untracked files are
//! synthesized per-file like freeze (unavoidable: `--no-index` is per path),
//! capped. Total patch bytes capped at 10 MB (freeze `MAX_TOTAL_PATCH_BYTES`).
//! All git runs off the async workers via `run_blocking` at the call site.
//!
//! Divergence D-VCS-NOAPPLY: `/vcs/apply` is not implemented (write path; the
//! web bundle has zero call sites — PLAN §17).

use serde_json::{Value, json};
use std::process::Command;

const MAX_TOTAL_PATCH_BYTES: usize = 10_000_000;
/// Full context, matching freeze PATCH_CONTEXT_LINES (2^31-1) semantics when
/// the caller passes no `context`.
const FULL_CONTEXT: &str = "2147483647";

fn run_git(dir: &str, args: &[&str]) -> Option<(String, i32)> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Some((text, out.status.code().unwrap_or(-1)))
}

/// `git status --porcelain=v1 -z --untracked-files=all --no-renames`.
/// Returns (file, code2) where code2 is the two-char XY.
fn status_entries(dir: &str) -> Vec<(String, String)> {
    let Some((text, _)) = run_git(
        dir,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--no-renames",
            "-z",
            "--",
            ".",
        ],
    ) else {
        return vec![];
    };
    let mut out = Vec::new();
    for item in text.split('\0').filter(|s| !s.is_empty()) {
        if item.len() < 3 {
            continue;
        }
        let code = item[..2].to_string();
        let file = item[3..].to_string();
        out.push((file, code));
    }
    out
}

/// `git diff --numstat -z <ref>` → file → (additions, deletions).
fn numstat(dir: &str, ref_: &str) -> std::collections::HashMap<String, (i64, i64)> {
    let Some((text, _)) = run_git(
        dir,
        &[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--numstat",
            "-z",
            ref_,
            "--",
            ".",
        ],
    ) else {
        return std::collections::HashMap::new();
    };
    let mut map = std::collections::HashMap::new();
    // -z format: "add\tdel\tfile\0" (renames would add a second path, but
    // --no-renames suppresses them)
    for item in text.split('\0').filter(|s| !s.is_empty()) {
        let mut it = item.splitn(3, '\t');
        let (Some(a), Some(d), Some(f)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let add = a.parse::<i64>().unwrap_or(0);
        let del = d.parse::<i64>().unwrap_or(0);
        map.insert(f.to_string(), (add, del));
    }
    map
}

fn kind(code: &str) -> &'static str {
    // freeze kind(code): A→added, D→deleted, else modified
    if code.starts_with('A') || code == "??" {
        "added"
    } else if code.starts_with('D') {
        "deleted"
    } else {
        "modified"
    }
}

fn has_head(dir: &str) -> bool {
    run_git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map(|(_, c)| c == 0)
        .unwrap_or(false)
}

pub fn info(dir: &str) -> Value {
    let branch = run_git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|(t, c)| *c == 0 && !t.trim().is_empty())
        .map(|(t, _)| t.trim().to_string());
    let default_branch = run_git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .filter(|(t, c)| *c == 0 && !t.trim().is_empty())
        .map(|(t, _)| {
            t.trim()
                .trim_start_matches("refs/remotes/origin/")
                .to_string()
        });
    json!({"branch": branch, "default_branch": default_branch})
}

fn is_git(dir: &str) -> bool {
    run_git(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|(t, c)| c == 0 && t.trim() == "true")
        .unwrap_or(false)
}

pub fn status(dir: &str) -> Value {
    if !is_git(dir) {
        return Value::Array(vec![]);
    }
    let ref_ = if has_head(dir) { "HEAD" } else { "" };
    let stats = if ref_.is_empty() {
        std::collections::HashMap::new()
    } else {
        numstat(dir, ref_)
    };
    let mut items: Vec<(String, String)> = status_entries(dir);
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let arr: Vec<Value> = items
        .into_iter()
        .map(|(file, code)| {
            let (a, d) = stats.get(&file).copied().unwrap_or_else(|| {
                if code == "??" {
                    stat_untracked(dir, &file)
                } else {
                    (0, 0)
                }
            });
            json!({"file": file, "additions": a, "deletions": d, "status": kind(&code)})
        })
        .collect();
    Value::Array(arr)
}

/// `git diff --no-index --numstat -- /dev/null <file>` → (adds, dels).
fn stat_untracked(dir: &str, file: &str) -> (i64, i64) {
    let Some((text, _)) = run_git(
        dir,
        &["diff", "--no-index", "--numstat", "--", "/dev/null", file],
    ) else {
        return (0, 0);
    };
    let line = text.lines().next().unwrap_or("");
    let mut it = line.splitn(3, '\t');
    let (Some(a), Some(d)) = (it.next(), it.next()) else {
        return (0, 0);
    };
    (a.parse::<i64>().unwrap_or(0), d.parse::<i64>().unwrap_or(0))
}

/// One batched tracked patch, split by `diff --git` headers → file→patch.
fn tracked_patches(
    dir: &str,
    ref_: &str,
    context: &str,
) -> (std::collections::HashMap<String, String>, bool) {
    let Some((text, _)) = run_git(
        dir,
        &[
            "diff",
            "--patch",
            "--no-ext-diff",
            "--no-renames",
            &format!("--unified={context}"),
            ref_,
            "--",
            ".",
        ],
    ) else {
        return (std::collections::HashMap::new(), false);
    };
    // split on "diff --git "
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut cur_file: Option<String> = None;
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(f) = cur_file.take() {
                map.entry(f).or_default().push_str(&cur);
                cur.clear();
            }
            cur_file = parse_git_header(line);
        }
        cur.push_str(line);
    }
    if let Some(f) = cur_file {
        map.entry(f).or_default().push_str(&cur);
    }
    // detect truncation cheaply: freeze caps by bytes; we cap at the caller
    (map, false)
}

/// Parse the destination path from a `diff --git a/x b/x` header line.
fn parse_git_header(line: &str) -> Option<String> {
    let rest = line.trim_end().strip_prefix("diff --git ")?;
    // find " b/" separator (unquoted)
    let idx = rest.find(" b/")?;
    Some(rest[idx + 3..].to_string())
}

pub fn diff_raw(dir: &str) -> String {
    if !is_git(dir) {
        return String::new();
    }
    let ref_ = if has_head(dir) { "HEAD" } else { "" };
    let mut parts: Vec<String> = Vec::new();
    if !ref_.is_empty()
        && let Some((text, _)) = run_git(
            dir,
            &[
                "diff",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                &format!("--unified={FULL_CONTEXT}"),
                ref_,
                "--",
                ".",
            ],
        )
    {
        parts.push(text);
    }
    for (file, code) in status_entries(dir) {
        if code == "??"
            && let Some((text, _)) = run_git(
                dir,
                &[
                    "diff",
                    "--no-index",
                    "--patch",
                    "--no-ext-diff",
                    "--no-renames",
                    &format!("--unified={FULL_CONTEXT}"),
                    "--",
                    "/dev/null",
                    &file,
                ],
            )
        {
            parts.push(text);
        }
    }
    parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/vcs/diff/raw` variant: same patch text but bounded by
/// `MAX_TOTAL_PATCH_BYTES` (the tracked `diff()` caps its aggregate; the raw
/// route concatenated the tracked patch AND one `--no-index` per untracked file
/// with no cap — an unbounded request-reachable allocation on a large dirty
/// tree). Truncation is at a char boundary; the caller marks it truncated.
pub fn diff_raw_bounded(dir: &str) -> (String, bool) {
    let full = diff_raw(dir);
    if full.len() <= MAX_TOTAL_PATCH_BYTES {
        return (full, false);
    }
    let mut cut = MAX_TOTAL_PATCH_BYTES;
    while cut > 0 && !full.is_char_boundary(cut) {
        cut -= 1;
    }
    (full[..cut].to_string(), true)
}

/// `mode`: git | branch. `context`: unified context lines (default full).
pub fn diff(dir: &str, mode: &str, context: Option<u32>) -> Value {
    if !is_git(dir) {
        return Value::Array(vec![]);
    }
    let ref_ = match mode {
        "git" => {
            if has_head(dir) {
                "HEAD".to_string()
            } else {
                String::new()
            }
        }
        "branch" => {
            let Some((def, c)) = run_git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]) else {
                return Value::Array(vec![]);
            };
            if c != 0 || def.trim().is_empty() {
                return Value::Array(vec![]);
            }
            let name = def
                .trim()
                .trim_start_matches("refs/remotes/origin/")
                .to_string();
            match run_git(dir, &["merge-base", "HEAD", &name]) {
                Some((mb, 0)) if !mb.trim().is_empty() => mb.trim().to_string(),
                _ => return Value::Array(vec![]),
            }
        }
        _ => return json!({"__error": "mode"}),
    };

    let ctx = context
        .map(|c| c.to_string())
        .unwrap_or_else(|| FULL_CONTEXT.into());
    let stats = if ref_.is_empty() {
        std::collections::HashMap::new()
    } else {
        numstat(dir, &ref_)
    };
    let (tracked, _trunc) = if ref_.is_empty() {
        (std::collections::HashMap::new(), false)
    } else {
        tracked_patches(dir, &ref_, &ctx)
    };

    // Build the file list: status entries (covers untracked), sorted.
    let mut items: Vec<(String, String)> = status_entries(dir);
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for (file, code) in items {
        let (a, d) = stats.get(&file).copied().unwrap_or_else(|| {
            if code == "??" {
                stat_untracked(dir, &file)
            } else {
                (0, 0)
            }
        });
        let patch = if total >= MAX_TOTAL_PATCH_BYTES {
            String::new()
        } else if let Some(p) = tracked.get(&file) {
            p.clone()
        } else if code == "??" {
            run_git(
                dir,
                &[
                    "diff",
                    "--no-index",
                    "--patch",
                    "--no-ext-diff",
                    "--no-renames",
                    &format!("--unified={ctx}"),
                    "--",
                    "/dev/null",
                    &file,
                ],
            )
            .map(|(t, _)| t)
            .unwrap_or_default()
        } else {
            String::new()
        };
        total += patch.len();
        out.push(json!({
            "file": file,
            "patch": patch,
            "additions": a,
            "deletions": d,
            "status": kind(&code),
        }));
    }
    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_mapping() {
        assert_eq!(kind("A "), "added");
        assert_eq!(kind("D "), "deleted");
        assert_eq!(kind("M "), "modified");
        assert_eq!(kind("??"), "added");
    }

    #[test]
    fn header_parse() {
        assert_eq!(
            parse_git_header("diff --git a/src/x.rs b/src/x.rs\n"),
            Some("src/x.rs".into())
        );
        assert_eq!(parse_git_header("not a header"), None);
    }
}
