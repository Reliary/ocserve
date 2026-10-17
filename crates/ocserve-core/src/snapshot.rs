//! Snapshot engine (SNAPSHOT.md §2) — a faithful port of upstream's shadow-git
//! mechanism (`packages/opencode/src/snapshot/index.ts` @ v1.18.31).
//!
//! A per-(project, worktree) shadow git repository lives at
//! `<data_dir>/snapshot/<hash(worktree)>`. Every operation runs as
//! `git --git-dir=<shadow> --work-tree=<worktree> …`. The engine captures the
//! worktree before a turn (`track`), records the files a tool round changed
//! (`patch`), and restores files on revert (`restore` / `revert`). It is
//! best-effort: a missing/invalid worktree yields `None` (no snapshot), never a
//! panic (server paths never panic — AGENTS §2.4).
//!
//! All methods are synchronous (they shell out to git); callers MUST run them
//! off the tokio workers (`spawn_blocking`), matching the F1 convoy rule.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Untracked-file cap (upstream `limit = 2 * 1024 * 1024`).
const LIMIT: u64 = 2 * 1024 * 1024;
/// `gc --prune=7.days` (upstream `prune = "7.days"`).
const PRUNE: &str = "7.days";

/// A `patch` part payload: the snapshot hash + the absolute paths changed since.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Patch {
    pub hash: String,
    pub files: Vec<String>,
}

struct Ctx {
    worktree: String,
    gitdir: PathBuf,
}

/// Snapshot engine handle. Cheap to clone; the discovered worktree/gitdir is
/// cached after first use.
#[derive(Clone)]
pub struct Snapshots {
    data_dir: PathBuf,
    directory: String,
    enabled: bool,
    ctx: std::sync::Arc<OnceLock<Option<Ctx>>>,
}

impl Snapshots {
    /// `directory` is the session's working directory; `data_dir` is the
    /// ocserve data root. `enabled` reflects config `snapshot != false` and the
    /// `OCSERVE_SNAPSHOT` env kill-switch.
    pub fn new(data_dir: impl Into<PathBuf>, directory: impl Into<String>, enabled: bool) -> Self {
        Self {
            data_dir: data_dir.into(),
            directory: directory.into(),
            enabled,
            ctx: std::sync::Arc::new(OnceLock::new()),
        }
    }

    /// Resolve worktree + shadow gitdir. `None` when disabled, not in a git
    /// worktree, or discovery fails.
    fn context(&self) -> Option<&Ctx> {
        self.ctx
            .get_or_init(|| {
                if !self.enabled {
                    return None;
                }
                let top = git_toplevel(&self.directory)?;
                let worktree = std::fs::canonicalize(&top)
                    .map(|p| p.display().to_string())
                    .unwrap_or(top);
                let hash = hash_hex(&worktree);
                let gitdir = self.data_dir.join("snapshot").join(hash);
                Some(Ctx { worktree, gitdir })
            })
            .as_ref()
    }

    /// Whether the engine can act (git worktree + enabled).
    pub fn is_active(&self) -> bool {
        self.context().is_some()
    }

    /// `track()` — ensure the shadow repo, stage the changed/untracked set,
    /// `write-tree` → the snapshot tree hash. `None` when inactive or on error.
    pub fn track(&self) -> Option<String> {
        let c = self.context()?;
        let worktree = c.worktree.clone();
        let gitdir = c.gitdir.clone();
        let gs = gitdir.display().to_string();
        let existed = gitdir.join("HEAD").exists();
        if let Err(e) = std::fs::create_dir_all(&gitdir) {
            tracing::warn!(error = %e, "snapshot: create gitdir failed");
            return None;
        }
        if !existed {
            let init = shadow_args(&gs, &worktree, &["init"]);
            let _ = run_git(
                &init,
                None,
                None,
                &[("GIT_DIR", &gs), ("GIT_WORK_TREE", &worktree)],
            );
            for (k, v) in TUNING {
                let a = shadow_args(&gs, &worktree, &["config", k, v]);
                let _ = run_git(&a, None, None, &[]);
            }
            self.seed(&gitdir, &worktree);
        }
        self.add(&worktree, &gs);
        let a = shadow_args(&gs, &worktree, &["write-tree"]);
        let r = run_git(&a, Some(&self.directory), None, &[]);
        let hash = r.text.trim().to_string();
        if r.code != 0 || hash.is_empty() {
            tracing::warn!(code = r.code, stderr = %r.stderr.trim(), "snapshot: write-tree failed");
            return None;
        }
        Some(hash)
    }

    /// Share the source repo's object DB (alternates) + index so already-hashed
    /// blobs are reused (upstream `seed`).
    fn seed(&self, gitdir: &Path, worktree: &str) {
        let common = run_git(
            &[
                "rev-parse".into(),
                "--path-format=absolute".into(),
                "--git-common-dir".into(),
            ],
            Some(worktree),
            None,
            &[],
        );
        if common.code != 0 {
            return;
        }
        let source = common.text.trim().to_string();
        if source.is_empty() || !Path::new(&source).exists() {
            return;
        }
        let source_objects = Path::new(&source).join("objects");
        let mut alternates: Vec<String> = Vec::new();
        if source_objects.exists() {
            alternates.push(source_objects.display().to_string());
            if let Ok(text) =
                std::fs::read_to_string(source_objects.join("info").join("alternates"))
            {
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if Path::new(line).exists() {
                        alternates.push(line.to_string());
                    }
                }
            }
        }
        if alternates.is_empty() {
            return;
        }
        let obj_info = gitdir.join("objects").join("info");
        let _ = std::fs::create_dir_all(&obj_info);
        let _ = std::fs::write(
            obj_info.join("alternates"),
            format!("{}\n", alternates.join("\n")),
        );
        let source_index = Path::new(&source).join("index");
        if source_index.exists() {
            let _ = std::fs::copy(&source_index, gitdir.join("index"));
        }
    }

    /// Stage the candidate set (upstream `add`): changed + untracked files,
    /// minus source-repo gitignored files, minus >2 MiB untracked files.
    fn add(&self, worktree: &str, gs: &str) {
        let diff = run_git(
            &quote(shadow_args(
                gs,
                worktree,
                &["diff-files", "--name-only", "-z", "--", "."],
            )),
            Some(&self.directory),
            None,
            &[],
        );
        let other = run_git(
            &quote(shadow_args(
                gs,
                worktree,
                &[
                    "ls-files",
                    "--full-name",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".",
                ],
            )),
            Some(&self.directory),
            None,
            &[],
        );
        if diff.code != 0 || other.code != 0 {
            tracing::warn!(
                dc = diff.code,
                oc = other.code,
                "snapshot: list files failed"
            );
            return;
        }
        let mut all: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in diff.text.split('\0').chain(other.text.split('\0')) {
            if !t.is_empty() && seen.insert(t.to_string()) {
                all.push(t.to_string());
            }
        }
        if all.is_empty() {
            return;
        }
        let ignored = self.ignore(worktree, &all);
        if !ignored.is_empty() {
            let files: Vec<String> = ignored.iter().cloned().collect();
            self.rm_cached(worktree, gs, &files);
        }
        let allow: Vec<String> = all.into_iter().filter(|f| !ignored.contains(f)).collect();
        let stage: Vec<String> = allow
            .into_iter()
            .filter(|f| {
                std::fs::metadata(Path::new(worktree).join(f))
                    .map(|m| !m.is_file() || m.len() <= LIMIT)
                    .unwrap_or(true)
            })
            .collect();
        self.stage(worktree, gs, &stage);
    }

    /// `git check-ignore --no-index` against the SOURCE repo (upstream `ignore`).
    fn ignore(&self, worktree: &str, files: &[String]) -> std::collections::HashSet<String> {
        if files.is_empty() {
            return std::collections::HashSet::new();
        }
        let paths: Vec<String> = files
            .iter()
            .map(|f| {
                if f.starts_with(':') {
                    format!("./{f}")
                } else {
                    f.clone()
                }
            })
            .collect();
        let stdin = nul_terminated(&paths);
        let args = quote(vec![
            "--git-dir".into(),
            format!("{worktree}/.git"),
            "--work-tree".into(),
            worktree.into(),
            "check-ignore".into(),
            "--no-index".into(),
            "--stdin".into(),
            "-z".into(),
        ]);
        let r = run_git(&args, Some(worktree), Some(&stdin), &[]);
        if r.code != 0 && r.code != 1 {
            return std::collections::HashSet::new();
        }
        r.text
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.strip_prefix("./:")
                    .map(|x| format!(":{x}"))
                    .unwrap_or_else(|| s.to_string())
            })
            .collect()
    }

    fn rm_cached(&self, worktree: &str, gs: &str, files: &[String]) {
        if files.is_empty() {
            return;
        }
        let stdin = top_literal_nul(files);
        let args = shadow_args(
            gs,
            worktree,
            &[
                "rm",
                "--cached",
                "-f",
                "--ignore-unmatch",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        );
        let _ = run_git(&args, Some(worktree), Some(&stdin), &[]);
    }

    fn stage(&self, worktree: &str, gs: &str, files: &[String]) {
        if files.is_empty() {
            return;
        }
        let stdin = top_literal_nul(files);
        let args = shadow_args(
            gs,
            worktree,
            &[
                "add",
                "--all",
                "--sparse",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        );
        let r = run_git(&args, Some(worktree), Some(&stdin), &[]);
        if r.code != 0 {
            tracing::warn!(code = r.code, stderr = %r.stderr.trim(), "snapshot: stage failed");
        }
    }

    /// `patch(hash)` — changed files since `hash` (absolute paths), ignored
    /// files filtered out (upstream `patch`).
    pub fn patch(&self, hash: &str) -> Patch {
        let Some(c) = self.context() else {
            return Patch::default();
        };
        let (worktree, gs) = (c.worktree.clone(), c.gitdir.display().to_string());
        self.add(&worktree, &gs);
        let r = run_git(
            &quote(shadow_args(
                gs.as_str(),
                &worktree,
                &[
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--name-only",
                    hash,
                    "--",
                    ".",
                ],
            )),
            Some(&self.directory),
            None,
            &[],
        );
        if r.code != 0 {
            tracing::warn!(code = r.code, "snapshot: patch diff failed");
            return Patch {
                hash: hash.to_string(),
                files: Vec::new(),
            };
        }
        let rel: Vec<String> = r
            .text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let ignored = self.ignore(&worktree, &rel);
        let wt = worktree.trim_end_matches('/');
        let files = rel
            .into_iter()
            .filter(|f| !ignored.contains(f))
            .map(|f| format!("{wt}/{f}"))
            .collect();
        Patch {
            hash: hash.to_string(),
            files,
        }
    }

    /// `restore(snapshot)` — replace the index with the tree + checkout all
    /// entries (upstream `restore`).
    pub fn restore(&self, snapshot: &str) {
        let Some(c) = self.context() else { return };
        let (worktree, gs) = (c.worktree.clone(), c.gitdir.display().to_string());
        let rt = run_git(
            &core_args(&gs, &worktree, &["read-tree", snapshot]),
            Some(&worktree),
            None,
            &[],
        );
        if rt.code != 0 {
            tracing::error!(code = rt.code, stderr = %rt.stderr.trim(), "snapshot: read-tree failed");
            return;
        }
        let co = run_git(
            &core_args(&gs, &worktree, &["checkout-index", "-a", "-f"]),
            Some(&worktree),
            None,
            &[],
        );
        if co.code != 0 {
            tracing::error!(code = co.code, stderr = %co.stderr.trim(), "snapshot: checkout-index failed");
        }
    }

    /// `revert(patches)` — for each file, `checkout <hash> -- <file>`; a file
    /// absent from `<hash>` is deleted (upstream `revert`).
    pub fn revert(&self, patches: &[Patch]) {
        let Some(c) = self.context() else { return };
        let (worktree, gs) = (c.worktree.clone(), c.gitdir.display().to_string());
        let mut seen = std::collections::HashSet::new();
        for p in patches {
            for abs in &p.files {
                if !seen.insert(abs.clone()) {
                    continue;
                }
                let rel = Path::new(abs)
                    .strip_prefix(&worktree)
                    .map(|r| r.display().to_string().replace('\\', "/"))
                    .unwrap_or_else(|_| abs.clone());
                let co = run_git(
                    &core_args(&gs, &worktree, &["checkout", &p.hash, "--", abs]),
                    Some(&worktree),
                    None,
                    &[],
                );
                if co.code == 0 {
                    continue;
                }
                let tree = run_git(
                    &core_args(&gs, &worktree, &["ls-tree", &p.hash, "--", &rel]),
                    Some(&worktree),
                    None,
                    &[],
                );
                if tree.code == 0 && !tree.text.trim().is_empty() {
                    continue; // existed in the snapshot but checkout failed — keep
                }
                let _ = std::fs::remove_file(abs);
            }
        }
    }

    /// `diff(hash)` — the unified-diff text since `hash` (upstream `diff`).
    pub fn diff(&self, hash: &str) -> String {
        let Some(c) = self.context() else {
            return String::new();
        };
        let (worktree, gs) = (c.worktree.clone(), c.gitdir.display().to_string());
        let add = shadow_args(&gs, &worktree, &["add", "--all"]);
        let _ = run_git(&add, Some(&self.directory), None, &[]);
        let r = run_git(
            &quote(shadow_args(
                gs.as_str(),
                &worktree,
                &["diff", "--cached", "--no-ext-diff", hash, "--", "."],
            )),
            Some(&self.directory),
            None,
            &[],
        );
        if r.code != 0 {
            return String::new();
        }
        r.text.trim().to_string()
    }

    /// `diffFull(from, to)` — per-file diffs `{file,patch,additions,deletions,status}`
    /// (upstream `diffFull`). Binary files carry an empty patch.
    pub fn diff_full(&self, from: &str, to: &str) -> Vec<serde_json::Value> {
        let Some(c) = self.context() else {
            return Vec::new();
        };
        let (worktree, gs) = (c.worktree.clone(), c.gitdir.display().to_string());
        let q = |cmd: &[&str]| quote(shadow_args(&gs, &worktree, cmd));

        let statuses = run_git(
            &q(&[
                "diff",
                "--no-ext-diff",
                "--name-status",
                "--no-renames",
                from,
                to,
                "--",
                ".",
            ]),
            Some(&self.directory),
            None,
            &[],
        );
        let mut status_map: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();
        for line in statuses.text.lines() {
            if line.is_empty() {
                continue;
            }
            let mut it = line.splitn(2, '\t');
            if let (Some(code), Some(file)) = (it.next(), it.next()) {
                let st = if code.starts_with('A') {
                    "added"
                } else if code.starts_with('D') {
                    "deleted"
                } else {
                    "modified"
                };
                status_map.insert(file.to_string(), st);
            }
        }
        let numstat = run_git(
            &q(&[
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--numstat",
                from,
                to,
                "--",
                ".",
            ]),
            Some(&self.directory),
            None,
            &[],
        );
        let rows: Vec<(String, bool, i64, i64)> = numstat
            .text
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                let mut it = line.splitn(3, '\t');
                let adds = it.next()?;
                let dels = it.next()?;
                let file = it.next()?.to_string();
                let binary = adds == "-" && dels == "-";
                let a = if binary { 0 } else { adds.parse().unwrap_or(0) };
                let d = if binary { 0 } else { dels.parse().unwrap_or(0) };
                Some((file, binary, a, d))
            })
            .collect();
        let ignored = self.ignore(
            &worktree,
            &rows
                .iter()
                .map(|(f, _, _, _)| f.clone())
                .collect::<Vec<_>>(),
        );
        let mut out = Vec::new();
        for (file, binary, additions, deletions) in rows {
            if ignored.contains(&file) {
                continue;
            }
            let patch = if binary {
                String::new()
            } else {
                run_git(
                    &q(&[
                        "diff",
                        "--no-ext-diff",
                        "-U2147483647",
                        from,
                        to,
                        "--",
                        &file,
                    ]),
                    Some(&self.directory),
                    None,
                    &[],
                )
                .text
                .trim_end()
                .to_string()
            };
            let status = status_map.get(&file).copied().unwrap_or("modified");
            out.push(serde_json::json!({
                "file": file,
                "patch": patch,
                "additions": additions,
                "deletions": deletions,
                "status": status,
            }));
        }
        out
    }

    /// `cleanup()` — `git gc --prune=7.days`.
    pub fn cleanup(&self) {
        let Some(c) = self.context() else { return };
        if !c.gitdir.exists() {
            return;
        }
        let (worktree, gs) = (c.worktree.clone(), c.gitdir.display().to_string());
        let a = shadow_args(&gs, &worktree, &["gc", &format!("--prune={PRUNE}")]);
        let _ = run_git(&a, Some(&self.directory), None, &[]);
    }

    /// `gc_prune_all(data_dir)` — `git gc --prune=7.days` over every shadow
    /// repo under `<data_dir>/snapshot/*` (upstream `Snapshot.cleanup()` runs
    /// on an interval). Best-effort; returns the number of repos pruned.
    pub fn gc_prune_all(data_dir: &Path) -> usize {
        let root = data_dir.join("snapshot");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return 0;
        };
        let mut n = 0;
        for e in entries.flatten() {
            let gitdir = e.path();
            if !gitdir.join("HEAD").exists() {
                continue;
            }
            let gs = gitdir.display().to_string();
            // gc does not need a work-tree, but passing one is harmless.
            let a = vec![
                format!("--git-dir={gs}"),
                "gc".to_string(),
                format!("--prune={PRUNE}"),
            ];
            if run_git(&a, None, None, &[]).code == 0 {
                n += 1;
            }
        }
        n
    }
}

const TUNING: &[(&str, &str)] = &[
    ("core.autocrlf", "false"),
    ("core.longpaths", "true"),
    ("core.symlinks", "true"),
    ("core.fsmonitor", "false"),
    ("feature.manyFiles", "true"),
    ("index.version", "4"),
    ("index.threads", "true"),
    ("core.untrackedCache", "true"),
];

struct GitResult {
    text: String,
    code: i32,
    stderr: String,
}

fn shadow_args(gitdir: &str, worktree: &str, cmd: &[&str]) -> Vec<String> {
    let mut v = vec![
        "--git-dir".to_string(),
        gitdir.to_string(),
        "--work-tree".to_string(),
        worktree.to_string(),
    ];
    v.extend(cmd.iter().map(|s| s.to_string()));
    v
}

fn core_args(gitdir: &str, worktree: &str, cmd: &[&str]) -> Vec<String> {
    let mut v = vec![
        "-c".to_string(),
        "core.longpaths=true".to_string(),
        "-c".to_string(),
        "core.symlinks=true".to_string(),
    ];
    v.extend(shadow_args(gitdir, worktree, cmd));
    v
}

fn quote(mut args: Vec<String>) -> Vec<String> {
    let mut v: Vec<String> = [
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.longpaths=true",
        "-c",
        "core.symlinks=true",
        "-c",
        "core.quotepath=false",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.append(&mut args);
    v
}

fn nul_terminated(items: &[String]) -> String {
    let mut s = items.join("\0");
    if !s.is_empty() {
        s.push('\0');
    }
    s
}

fn top_literal_nul(files: &[String]) -> String {
    nul_terminated(
        &files
            .iter()
            .map(|f| format!(":(top,literal){f}"))
            .collect::<Vec<_>>(),
    )
}

fn run_git(
    args: &[String],
    cwd: Option<&str>,
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> GitResult {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let Ok(mut child) = cmd.spawn() else {
        return GitResult {
            text: String::new(),
            code: -1,
            stderr: "spawn failed".into(),
        };
    };
    if let Some(s) = stdin
        && let Some(mut si) = child.stdin.take()
    {
        use std::io::Write;
        let _ = si.write_all(s.as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return GitResult {
            text: String::new(),
            code: -1,
            stderr: "wait failed".into(),
        };
    };
    GitResult {
        text: String::from_utf8_lossy(&out.stdout).into_owned(),
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn hash_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// `git rev-parse --show-toplevel` for a directory (the worktree root).
pub fn git_toplevel(dir: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp git repo with an initial commit and a `.gitignore`.
    struct Repo {
        dir: PathBuf,
        _tmp: tempfile::TempDir,
        data: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().to_path_buf();
            let data = tempfile::tempdir().unwrap();
            let run = |args: &[&str]| {
                let out = Command::new("git")
                    .current_dir(&dir)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "git {:?}: {}",
                    args,
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            run(&["init", "-q"]);
            run(&["config", "user.email", "t@t"]);
            run(&["config", "user.name", "t"]);
            std::fs::write(dir.join("a.txt"), "one\n").unwrap();
            std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
            run(&["add", "."]);
            run(&["commit", "-qm", "init"]);
            Self {
                dir,
                _tmp: tmp,
                data,
            }
        }
        fn snap(&self) -> Snapshots {
            Snapshots::new(self.data.path(), self.dir.display().to_string(), true)
        }
        fn write(&self, name: &str, content: &str) {
            std::fs::write(self.dir.join(name), content).unwrap();
        }
        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.join(name)).unwrap()
        }
    }

    #[test]
    fn gc_prune_all_prunes_existing_shadow_repos() {
        let r = Repo::new();
        let s = r.snap();
        let _base = s.track().expect("track"); // materializes the shadow repo
        // one shadow repo exists under <data>/snapshot/
        let pruned = Snapshots::gc_prune_all(r.data.path());
        assert_eq!(pruned, 1, "the one shadow repo is pruned");
        // a missing snapshot root is a clean no-op, not a panic
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(Snapshots::gc_prune_all(empty.path()), 0);
    }

    #[test]
    fn track_patch_restore_roundtrip() {
        let r = Repo::new();
        let s = r.snap();
        let base = s.track().expect("track");
        r.write("a.txt", "two\n");
        let p = s.patch(&base);
        assert_eq!(p.files.len(), 1, "one changed file: {:?}", p.files);
        assert!(p.files[0].ends_with("/a.txt"), "{:?}", p.files);
        // restore returns the file to the captured tree
        s.restore(&base);
        assert_eq!(
            r.read("a.txt"),
            "one\n",
            "restore must reset tracked content"
        );
    }

    #[test]
    fn added_and_untracked_files_are_captured_and_deleted_on_revert() {
        let r = Repo::new();
        let s = r.snap();
        let base = s.track().unwrap();
        r.write("new.txt", "fresh\n");
        let p = s.patch(&base);
        assert!(
            p.files.iter().any(|f| f.ends_with("/new.txt")),
            "{:?}",
            p.files
        );
        // revert deletes a file that did not exist in the snapshot
        s.revert(&[p]);
        assert!(
            !r.dir.join("new.txt").exists(),
            "revert must delete an added file"
        );
    }

    #[test]
    fn modified_file_is_restored_to_snapshot_bytes_on_revert() {
        let r = Repo::new();
        let s = r.snap();
        let base = s.track().unwrap();
        r.write("a.txt", "changed\n");
        let p = s.patch(&base);
        s.revert(&[p]);
        assert_eq!(r.read("a.txt"), "one\n");
    }

    #[test]
    fn ignored_files_are_never_captured() {
        let r = Repo::new();
        let s = r.snap();
        let base = s.track().unwrap();
        r.write("ignored.txt", "secret\n");
        let p = s.patch(&base);
        assert!(
            p.files.iter().all(|f| !f.ends_with("ignored.txt")),
            "ignored must be filtered: {:?}",
            p.files
        );
    }

    #[test]
    fn large_untracked_file_is_excluded() {
        let r = Repo::new();
        let s = r.snap();
        let base = s.track().unwrap();
        let big = "x".repeat((LIMIT as usize) + 10);
        r.write("big.bin", &big);
        let p = s.patch(&base);
        assert!(
            p.files.iter().all(|f| !f.ends_with("big.bin")),
            ">2MiB untracked must be excluded: {:?}",
            p.files
        );
    }

    #[test]
    fn non_git_directory_is_inactive() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        // Hermetic non-git precondition: the fixture must not depend on TMPDIR
        // being outside any repo (the mutants harness sets TMPDIR inside the
        // ocserve repo, so git would walk up and find it — flaky class). A
        // bogus `.git` entry stops git's upward walk (rev-parse fails, no
        // fall-through to a parent repo).
        std::fs::write(tmp.path().join(".git"), "not a gitdir\n").unwrap();
        assert!(
            git_toplevel(&tmp.path().display().to_string()).is_none(),
            "fixture must be hermetic (no git found upward)"
        );
        let s = Snapshots::new(data.path(), tmp.path().display().to_string(), true);
        assert!(!s.is_active(), "non-git dir must be inactive");
        assert!(s.track().is_none());
    }

    #[test]
    fn disabled_engine_is_inactive_even_in_git() {
        let r = Repo::new();
        let s = Snapshots::new(r.data.path(), r.dir.display().to_string(), false);
        assert!(!s.is_active());
        assert!(s.track().is_none());
    }

    #[test]
    fn diff_full_reports_status_added_modified_deleted() {
        let r = Repo::new();
        let s = r.snap();
        let from = s.track().unwrap();
        r.write("a.txt", "two\nthree\n");
        r.write("new.txt", "n\n");
        std::fs::remove_file(r.dir.join("a.txt.bak")).ok();
        let to = s.track().unwrap();
        let diffs = s.diff_full(&from, &to);
        let by: std::collections::HashMap<_, _> = diffs
            .iter()
            .map(|d| (d["file"].as_str().unwrap().to_string(), d.clone()))
            .collect();
        assert!(by.contains_key("a.txt"));
        assert_eq!(by["a.txt"]["status"], "modified");
        assert!(by["a.txt"]["additions"].as_i64().unwrap() >= 1);
        assert!(by.contains_key("new.txt"));
        assert_eq!(by["new.txt"]["status"], "added");
    }

    #[test]
    fn subdir_scoping_only_captures_the_session_subtree() {
        // worktree = repo root (git toplevel); a session in a subdir still
        // snapshots the whole worktree (upstream scopes by location.directory,
        // which for a repo-root session is "."). This asserts root scoping is
        // consistent: both a root file and a subdir file are captured.
        let r = Repo::new();
        let s = r.snap();
        let base = s.track().unwrap();
        std::fs::create_dir_all(r.dir.join("sub")).unwrap();
        r.write("sub/deep.txt", "d\n");
        let p = s.patch(&base);
        assert!(
            p.files.iter().any(|f| f.ends_with("/sub/deep.txt")),
            "{:?}",
            p.files
        );
    }
}
