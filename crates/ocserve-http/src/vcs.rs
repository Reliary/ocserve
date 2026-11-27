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
//! `/vcs/apply` applies a patch to the worktree via `git apply -` (SNAPSHOT
//! §5); `vcs::apply` is the shell-out used by the v2 handler.

use serde_json::{Value, json};

const MAX_TOTAL_PATCH_BYTES: usize = 10_000_000;
/// Full context, matching freeze PATCH_CONTEXT_LINES (2^31-1) semantics when
/// the caller passes no `context`.
const FULL_CONTEXT: &str = "2147483647";

/// Repo config a *client-named* directory can use to run a program during a
/// read-only-looking git command. `--no-ext-diff` covers `diff.external` on
/// every diff call below, but two vectors remain and both are repo-controlled:
///
///   * `core.fsmonitor=<cmd>`: git spawns it for `status`/`diff` (measured on
///     git 2.54: a hook script ran during `git status --porcelain`).
///   * `diff.<driver>.textconv=<cmd>`: runs on a plain `git diff --patch`;
///     `--no-ext-diff` does NOT disable it (measured: it ran on `git diff --`,
///     and did not run with `--stat`).
///
/// Since #64/#65/#66 let the client choose the directory, git now runs in
/// directories the caller names, so both are neutralised on every invocation.
/// `-c` overrides repo config; the environment cannot re-enable them.
const GIT_SAFETY: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// `--no-textconv` is a DIFF option, not a global flag: `git --no-textconv
/// status` exits 129 and silently breaks every route (measured; the first
/// version of this guard did exactly that). It therefore rides on the diff
/// invocations, next to the `--no-ext-diff` that already neutralises
/// `diff.external`.
const DIFF_SAFETY: &[&str] = &["--no-ext-diff", "--no-textconv"];

/// `run_git` for a call that needs bytes on stdin (only `git apply`).
///
/// Same `GIT_SAFETY` flags and the same deadline as `run_git`; the difference
/// is the stdin pipe, which is written on its own thread so the child cannot
/// block on a full stdin while we block reading its stdout (issue #80's pipe
/// lesson, applied here).
fn run_git_with_stdin(dir: &str, args: &[&str], stdin: &[u8]) -> Option<bool> {
    use std::io::Write;
    let mut cmd = ocserve_store::cgroup::command("git", "tools");
    cmd.current_dir(dir)
        .args(GIT_SAFETY)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let label = format!("git {}", args.first().copied().unwrap_or(""));
    // `run_with_deadline` pipes stdout/stderr and drains them; it does not
    // touch stdin, so hand it a child whose stdin we feed ourselves.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("{label} failed to spawn: {e}");
            return None;
        }
    };
    let payload = stdin.to_vec();
    let writer = child.stdin.take().map(|mut si| {
        std::thread::spawn(move || {
            let _ = si.write_all(&payload);
            // Drop closes the pipe: git needs EOF to finish the patch.
        })
    });
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_t = out.take().map(|mut p| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut v = Vec::new();
            let _ = p.read_to_end(&mut v);
            v
        })
    });
    let err_t = err.take().map(|mut p| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut v = Vec::new();
            let _ = p.read_to_end(&mut v);
            v
        })
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("{label} wait failed: {e}");
                return None;
            }
        }
        if start.elapsed() >= GIT_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            ocserve_metrics::labeled_counter("ocserve_vcs_git_errors_total", "op=\"apply\"", 1);
            tracing::warn!(
                "{label} exceeded {}s and was killed",
                GIT_DEADLINE.as_secs()
            );
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    if let Some(w) = writer {
        let _ = w.join();
    }
    let _ = out_t.and_then(|t| t.join().ok());
    let _ = err_t.and_then(|t| t.join().ok());
    if !status.success() {
        ocserve_metrics::labeled_counter("ocserve_vcs_git_errors_total", "op=\"apply\"", 1);
    }
    Some(status.success())
}

/// Wall-clock cap for one git call. Generous for a real repository (a cold
/// `git status` on a large tree is seconds, not minutes) but finite, because
/// the blocking pool is shared with every store route: an unbounded `git`
/// against a huge or network-mounted repo would starve the whole server.
const GIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

fn run_git(dir: &str, args: &[&str]) -> Option<(String, i32)> {
    // Containment: a vcs walk on a huge worktree is tool work, not server work.
    let mut cmd = ocserve_store::cgroup::command("git", "tools");
    cmd.current_dir(dir)
        .args(GIT_SAFETY)
        .args(args)
        .stdin(std::process::Stdio::null());
    let label = format!("git {}", args.first().copied().unwrap_or(""));
    let out = match ocserve_store::platform::run_with_deadline(cmd, GIT_DEADLINE, &label) {
        Ok(o) => o,
        Err(e) => {
            // Silent-vs-failed class: an absent/unreadable git produced "clean"
            // results indistinguishable from a real empty one. Count it.
            ocserve_metrics::labeled_counter(
                "ocserve_vcs_git_errors_total",
                &format!("op=\"{}\"", args.first().copied().unwrap_or("")),
                1,
            );
            tracing::warn!("{label} failed: {e}");
            return None;
        }
    };
    if !out.status.success() {
        ocserve_metrics::labeled_counter(
            "ocserve_vcs_git_errors_total",
            &format!("op=\"{}\"", args.first().copied().unwrap_or("")),
            1,
        );
    }
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
            DIFF_SAFETY[0],
            DIFF_SAFETY[1],
            "--no-renames",
            "--numstat",
            "-z",
            "--end-of-options",
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

/// Maximum patch size accepted by `apply` (SNAPSHOT §5). A larger patch is
/// refused as `not-clean` (upstream `git.applyPatch` would also fail/misbehave
/// on an absurd patch; this bounds the write).
const MAX_APPLY_PATCH_BYTES: usize = 64 * 1024 * 1024;

/// `git apply -` in `dir` with `patch` on stdin (upstream `Git.applyPatch`).
/// `None` when `dir` is not inside a git work tree; `Some(ok)` otherwise.
pub fn apply(dir: &str, patch: &str) -> Option<bool> {
    if patch.len() > MAX_APPLY_PATCH_BYTES {
        return Some(false);
    }
    if !is_git(dir) {
        return None;
    }
    // Hardened the same way as every other git call (reviews 2026-11-26):
    // repo-config execution is neutralised AND the call is deadline-bounded.
    // `git apply` takes its patch on stdin, so it uses the streaming variant.
    let out = run_git_with_stdin(dir, &["apply", "-"], patch.as_bytes())?;
    Some(out)
}

fn has_head(dir: &str) -> bool {
    run_git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map(|(_, c)| c == 0)
        .unwrap_or(false)
}

/// Git worktree toplevel for `dir` (upstream `project.worktree`). None when
/// `dir` is not inside a git work tree.
pub fn toplevel(dir: &std::path::Path) -> Option<String> {
    let d = dir.to_str()?;
    let (t, c) = run_git(d, &["rev-parse", "--show-toplevel"])?;
    (c == 0 && !t.trim().is_empty()).then(|| t.trim().to_string())
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

/// v2 `Vcs.Info` projection (`{branch:{current, default}}`): same git
/// queries as `info`, per-contract shape (v1 wants string branch — the two
/// contracts conflict on the key, so no union; see v2::vcs_get).
pub fn info_v2(dir: &str) -> Value {
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
    json!({
        "branch": {
            "current": branch,
            "default": default_branch,
        }
    })
}

/// Base ref for diffs (`Vcs.Base{name, ref, source}`): the merge-base
/// against the default branch, else HEAD. Null when not a git repo (never
/// fabricated — the contract's `data` allows null).
pub fn base(dir: &str) -> Value {
    if !is_git(dir) {
        return Value::Null;
    }
    let default_branch = run_git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .filter(|(t, c)| *c == 0 && !t.trim().is_empty())
        .map(|(t, _)| {
            t.trim()
                .trim_start_matches("refs/remotes/origin/")
                .to_string()
        });
    // v2 `source` enum is reflog|default (contract): a merge-base against
    // the default branch reports "default"; HEAD fallback reports "reflog"
    // only when resolvable, else the null-data path above already returned.
    let (name, source) = match default_branch {
        Some(b) => (b, "default"),
        None => ("HEAD".to_string(), "reflog"),
    };
    let r = run_git(dir, &["merge-base", &name, "HEAD"])
        .filter(|(t, c)| *c == 0 && !t.trim().is_empty())
        .map(|(t, _)| t.trim().to_string())
        .or_else(|| {
            run_git(dir, &["rev-parse", "HEAD"])
                .filter(|(t, c)| *c == 0 && !t.trim().is_empty())
                .map(|(t, _)| t.trim().to_string())
        });
    match r {
        Some(sha) => json!({"name": name, "ref": sha, "source": source}),
        None => Value::Null,
    }
}

/// Branch list (`Vcs.BranchList` = string array): local branches via
/// `for-each-ref`. Empty (not an error) when not a git repo.
pub fn branches(dir: &str) -> Value {
    if !is_git(dir) {
        return Value::Array(vec![]);
    }
    run_git(
        dir,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .filter(|(_t, c)| *c == 0)
    .map(|(t, _)| {
        Value::Array(
            t.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(|l| json!(l))
                .collect(),
        )
    })
    .unwrap_or_else(|| Value::Array(vec![]))
}

/// Worktree list (`Worktree.List`): `git worktree list --porcelain` parsed
/// to `[{directory}]`. Empty when not a git repo (never an error — the
/// caller decides 404-vs-empty per contract).
pub fn worktrees(dir: &str) -> Value {
    if !is_git(dir) {
        return Value::Array(vec![]);
    }
    run_git(dir, &["worktree", "list", "--porcelain"])
        .filter(|(_, c)| *c == 0)
        .map(|(t, _)| {
            Value::Array(
                t.lines()
                    .filter_map(|l| l.strip_prefix("worktree "))
                    .map(|d| json!({"directory": d.trim()}))
                    .collect(),
            )
        })
        .unwrap_or_else(|| Value::Array(vec![]))
}

/// Create a worktree (`git worktree add [--detach] <dir> [<branch>]`).
/// Returns the directory on success, None with stderr on failure.
pub fn worktree_add(dir: &str, path: &str, branch: Option<&str>) -> Result<String, String> {
    if !is_git(dir) {
        return Err("not a git repository".to_string());
    }
    // A branch/ref or path beginning with `-` would be parsed by git as an
    // option; refuse the ref and pass the path after `--`.
    if branch.is_some_and(|b| b.starts_with('-')) {
        return Err("invalid branch name".to_string());
    }
    let mut args = vec!["worktree", "add"];
    if branch.is_some() {
        args.push("--detach");
    }
    args.push("--");
    args.push(path);
    if let Some(b) = branch {
        args.push(b);
    }
    match run_git(dir, &args) {
        Some((_, 0)) => Ok(path.to_string()),
        Some((t, _)) => Err(t.trim().to_string()),
        None => Err("git failed to spawn".to_string()),
    }
}

/// Remove a worktree (`git worktree remove [--force] <dir>`).
pub fn worktree_remove(dir: &str, path: &str, force: bool) -> Result<(), String> {
    if !is_git(dir) {
        return Err("not a git repository".to_string());
    }
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push("--");
    args.push(path);
    match run_git(dir, &args) {
        Some((_, 0)) => Ok(()),
        Some((t, _)) => Err(t.trim().to_string()),
        None => Err("git failed to spawn".to_string()),
    }
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
            DIFF_SAFETY[0],
            DIFF_SAFETY[1],
            "--no-renames",
            &format!("--unified={context}"),
            "--end-of-options",
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
                DIFF_SAFETY[0],
                DIFF_SAFETY[1],
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
                    DIFF_SAFETY[0],
                    DIFF_SAFETY[1],
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
    // v1 defaults: the mode-derived ref, untracked files included.
    diff_with(dir, mode, context, None, true)
}

/// `diff` plus the two v2 query parameters the shipped TUI sends.
///
/// * `base` — diff against this ref instead of the mode-derived one. The TUI's
///   diff viewer passes the branch the user selected, so ignoring it rendered
///   the HEAD diff for every selection.
/// * `untracked` — include untracked files (default true, the v1 behaviour).
pub fn diff_with(
    dir: &str,
    mode: &str,
    context: Option<u32>,
    base: Option<&str>,
    untracked: bool,
) -> Value {
    if !is_git(dir) {
        return Value::Array(vec![]);
    }
    let ref_ = match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => {
            // A caller-supplied ref is UNTRUSTED (it arrives on an
            // unauthenticated GET). `git diff <ref>` treats a leading `-` as an
            // OPTION, so `base=--output=/path` made the server write an
            // attacker-chosen file and `base=--ext-diff` re-enabled the repo's
            // external diff driver after `--no-ext-diff` (arbitrary command
            // execution where a driver is configured).
            //
            // Two layers, because either alone is easy to regress:
            //   1. reject anything that is not a plausible ref (no leading `-`,
            //      no control chars, no `..` traversal into a path);
            //   2. resolve it with `rev-parse --verify --end-of-options`, which
            //      treats the argument as a revision whatever it looks like, and
            //      reject an unknown ref instead of letting git guess.
            let Some(resolved) = resolve_ref(dir, b) else {
                return Value::Array(vec![]);
            };
            resolved
        }
        None => match diff_ref_for(dir, mode) {
            Some(r) => r,
            None => return Value::Array(vec![]),
        },
    };
    diff_body(dir, &ref_, context, untracked)
}

/// Validate and resolve a caller-supplied ref.
///
/// Returns the resolved object name, or `None` when the value is not a ref this
/// server will hand to git. `--end-of-options` is the load-bearing part: it
/// stops option parsing so a value like `--output=/x` can never be read as a
/// flag, even if the character filter is later relaxed.
fn resolve_ref(dir: &str, raw: &str) -> Option<String> {
    if raw.is_empty()
        || raw.starts_with('-')
        || raw.chars().any(|c| c.is_control() || c == '\0')
        || raw.contains("..")
    {
        return None;
    }
    let (out, code) = run_git(dir, &["rev-parse", "--verify", "--end-of-options", raw])?;
    if code != 0 {
        return None;
    }
    let resolved = out.trim();
    if resolved.is_empty() {
        return None;
    }
    Some(resolved.to_string())
}

/// The mode-derived ref (`git` → HEAD, `branch` → merge-base with origin/HEAD).
/// `None` means "no diff to show" (not a repo, no HEAD, no default branch).
fn diff_ref_for(dir: &str, mode: &str) -> Option<String> {
    let ref_ = match mode {
        "git" => {
            if has_head(dir) {
                "HEAD".to_string()
            } else {
                String::new()
            }
        }
        "branch" => {
            let (def, c) = run_git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"])?;
            if c != 0 || def.trim().is_empty() {
                return None;
            }
            let name = def
                .trim()
                .trim_start_matches("refs/remotes/origin/")
                .to_string();
            match run_git(dir, &["merge-base", "HEAD", &name]) {
                Some((mb, 0)) if !mb.trim().is_empty() => mb.trim().to_string(),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(ref_)
}

fn diff_body(dir: &str, ref_: &str, context: Option<u32>, untracked: bool) -> Value {
    let ctx = context
        .map(|c| c.to_string())
        .unwrap_or_else(|| FULL_CONTEXT.into());
    let stats = if ref_.is_empty() {
        std::collections::HashMap::new()
    } else {
        numstat(dir, ref_)
    };
    let (tracked, _trunc) = if ref_.is_empty() {
        (std::collections::HashMap::new(), false)
    } else {
        tracked_patches(dir, ref_, &ctx)
    };

    // Build the file list: status entries (covers untracked), sorted.
    let mut items: Vec<(String, String)> = status_entries(dir)
        .into_iter()
        .filter(|(_, code)| untracked || code != "??")
        .collect();
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
                    DIFF_SAFETY[0],
                    DIFF_SAFETY[1],
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
    /// A client-named directory (#64/#65/#66) must not be able to run a
    /// program through repo config. Two vectors, both measured on git 2.54:
    /// `core.fsmonitor` runs during `git status --porcelain`, and
    /// `diff.<driver>.textconv` runs on `git diff --patch` WITHOUT being
    /// suppressed by `--no-ext-diff`.
    ///
    /// Each vector carries its own POSITIVE control (bare `git`, no safety
    /// flags) so a "blocked" result cannot be a command that never ran.
    /// Planted control for the guard: remove the entry from `GIT_SAFETY` and
    /// the positive control passes while the guarded call creates the marker.
    fn sh(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    }

    fn arm_hook(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let marker = dir.join(name);
        let hook = dir.join(format!("{name}.sh"));
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch {}\necho '{{}}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        hook
    }

    #[test]
    fn git_safety_flags_block_repo_config_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        sh(&dir, &["init", "-q", "."]);

        // ---- core.fsmonitor ----
        let hook = arm_hook(&dir, "fsm");
        sh(&dir, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        // positive control: bare git must run it, or this fixture proves nothing
        sh(&dir, &["status", "--porcelain"]);
        let marker = dir.join("fsm");
        assert!(
            marker.exists(),
            "fixture invalid: bare git did not run core.fsmonitor"
        );
        std::fs::remove_file(&marker).unwrap();
        // guarded: our own path must not
        let _ = status_entries(dir.to_str().unwrap());
        assert!(
            !marker.exists(),
            "core.fsmonitor hook ran through run_git: a client-named directory can execute code"
        );

        // Isolate the next vector: a leftover fsmonitor hook reporting "no
        // changes" makes `git diff` trust the stale index and print nothing,
        // which read as "the textconv vector is absent". The positive control
        // caught it.
        sh(&dir, &["config", "--unset", "core.fsmonitor"]);

        // ---- diff.<driver>.textconv ----
        // The filter must CAT the file: a constant output makes both sides
        // identical, `git diff` prints nothing, and the vector looks absent.
        let cvt = dir.join("cvt.sh");
        std::fs::write(
            &cvt,
            format!(
                "#!/bin/sh\ntouch {}\ncat \"$1\"\n",
                dir.join("cvt").display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cvt, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("f.txt"), "a\n").unwrap();
        std::fs::write(dir.join(".gitattributes"), "*.txt diff=up\n").unwrap();
        for (k, v) in [
            ("user.email", "t@example.com"),
            ("user.name", "t"),
            ("diff.up.textconv", cvt.to_str().unwrap()),
        ] {
            sh(&dir, &["config", k, v]);
        }
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-qm", "init"]);
        std::fs::write(dir.join("f.txt"), "b\n").unwrap();

        // positive control: a bare patch diff runs the filter
        let bare = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["diff", "--patch", "--", "."])
            .output()
            .unwrap();
        assert!(!bare.stdout.is_empty(), "fixture invalid: bare diff empty");
        let marker2 = dir.join("cvt");
        assert!(
            marker2.exists(),
            "fixture invalid: bare git diff did not run the textconv filter"
        );
        std::fs::remove_file(&marker2).unwrap();

        let _ = diff_with(dir.to_str().unwrap(), "unstaged", None, None, false);
        let _ = diff_raw(dir.to_str().unwrap());
        assert!(
            !marker2.exists(),
            "textconv filter ran through run_git: --no-ext-diff does not suppress it"
        );
    }
}
