//! OpenCode v2 `patch` tool: the `*** Begin Patch` free-form patch language.
//!
//! Upstream replaces `edit`+`write` with `patch` for GPT-family models
//! (`tool/plugin/patch.ts` context hook, `patch.txt` for the grammar). The
//! parser here is strict: a context line that does not match the file is an
//! error, never a best-effort guess, because a fuzzy apply could silently put
//! the change somewhere else. Optional `*** Move to:` is supported.
//!
//! Format:
//! ```text
//! *** Begin Patch
//! *** Add File: hello.txt
//! +Hello world
//! *** Update File: src/app.py
//! *** Move to: src/main.py
//! @@ def greet():
//! -print("Hi")
//! +print("Hello, world!")
//! *** Delete File: obsolete.txt
//! *** End Patch
//! ```

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::ToolResult;

enum Change {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        ops: Vec<(char, String)>,
    },
}

fn parse(text: &str) -> Result<Vec<Change>> {
    let mut lines = text.lines().peekable();
    // Tolerate prose/fences before the envelope, like a model echoing the
    // worked example; the envelope itself must be well formed.
    for line in lines.by_ref() {
        if line.trim() == "*** Begin Patch" {
            break;
        }
    }
    let mut changes: Vec<Change> = Vec::new();
    let mut finished = false;
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        if trimmed == "*** End Patch" {
            finished = true;
            break;
        }
        if let Some(path) = trimmed.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                let next = lines.next().unwrap_or_default();
                let body = next
                    .strip_prefix('+')
                    .with_context(|| format!("Add File {path}: every line must start with '+'"))?;
                content.push_str(body);
                content.push('\n');
            }
            changes.push(Change::Add {
                path: path.trim().to_string(),
                content,
            });
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("*** Delete File: ") {
            if let Some(next) = lines.peek()
                && !next.starts_with("*** ")
            {
                bail!("Delete File {}: unexpected body line", path.trim());
            }
            changes.push(Change::Delete {
                path: path.trim().to_string(),
            });
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            let mut move_to = None;
            let mut ops: Vec<(char, String)> = Vec::new();
            while let Some(next) = lines.peek() {
                let next_trimmed = next.trim_end();
                if let Some(target) = next_trimmed.strip_prefix("*** Move to: ") {
                    move_to = Some(target.trim().to_string());
                    lines.next();
                    continue;
                }
                if next_trimmed.starts_with("*** ") {
                    break;
                }
                let next = lines.next().unwrap_or_default();
                if next.starts_with("@@") {
                    // Hunk header: context is informational; the following
                    // context lines carry the match.
                    continue;
                }
                if next == "\\ No newline at end of file" {
                    continue;
                }
                let mut chars = next.chars();
                let Some(op) = chars.next() else {
                    // An empty line is an empty context line.
                    ops.push((' ', String::new()));
                    continue;
                };
                match op {
                    ' ' | '-' | '+' => ops.push((op, chars.as_str().to_string())),
                    other => bail!(
                        "Update File {path}: unexpected line start {other:?} (must be ' ', '-' or '+')"
                    ),
                }
            }
            changes.push(Change::Update { path, move_to, ops });
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        bail!("unexpected patch line: {trimmed}");
    }
    if !finished {
        bail!("patch is missing \"*** End Patch\"");
    }
    if changes.is_empty() {
        bail!("patch contains no file operations");
    }
    Ok(changes)
}

/// Containment for every path the patch touches.
///
/// The v2 `patch` tool was added with no containment at all (`cwd.join` then
/// `fs::write`), so a `*** Add File: ../../etc/x` or an absolute path wrote
/// anywhere the process could. It now uses the same handle-verified primitive
/// as the file routes: every component is resolved beneath `cwd` with the
/// kernel's `RESOLVE_BENEATH`, and a symlink leaving the tree is rejected.
///
/// Returns the verified real path, which is what gets written, so there is no
/// TOCTOU window between the check and the write.
fn contained_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    use ocserve_store::containment::{OpenError, OpenKind, open_within};
    // A directory open proves containment for the target's parent; the leaf
    // itself may not exist yet (Add File), so verify the parent.
    let leaf = Path::new(path);
    let parent = leaf.parent().unwrap_or_else(|| Path::new("."));
    let parent_str = parent.to_string_lossy();
    let handle = if parent_str.is_empty() || parent_str == "." {
        open_within(cwd, ".", OpenKind::Dir)
    } else {
        open_within(cwd, &parent_str, OpenKind::Dir)
    }
    .map_err(|e: OpenError| {
        anyhow::anyhow!("patch path {path:?} is not inside the project: {e}")
    })?;
    let name = leaf
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("patch path {path:?} has no file name"))?;
    Ok(handle.path().join(name))
}

/// Remove a path already proven contained.
fn remove_contained(target: &Path) -> Result<()> {
    // Callers pass a path already returned by `contained_path`, i.e. the
    // kernel-verified real path.
    std::fs::remove_file(target).with_context(|| format!("delete {}", target.display())) // allow:raw-write
}

/// Refuse any path whose first component is `.git`.
///
/// Containment alone allows this (`.git/` is inside the project), but writing
/// `.git/hooks/pre-commit` is a code-execution primitive that fires on the
/// user's next commit, and no legitimate patch does it. Named divergence
/// `D-PATCH-NO-GIT-DIR`.
fn refuse_git_dir(path: &str) -> Result<()> {
    let first = Path::new(path)
        .components()
        .find_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .unwrap_or_default();
    if first == ".git" {
        bail!("patch may not write inside .git (D-PATCH-NO-GIT-DIR): {path}");
    }
    Ok(())
}

/// Write `body` to a path already proven contained, refusing to follow a
/// symlink at the leaf (writing through one is the escape class).
fn write_contained(target: &Path, body: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    use std::io::Write;
    let mut f = opts
        .open(target)
        .with_context(|| format!("write {}", target.display()))?;
    f.write_all(body)
        .with_context(|| format!("write {}", target.display()))?;
    Ok(())
}

/// Apply `ops` to `before`, returning the new content.
fn apply_ops(path: &str, before: &str, ops: &[(char, String)]) -> Result<String> {
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let mut out = String::with_capacity(before.len());
    let mut cursor = 0usize;
    for (op, text) in ops {
        match op {
            ' ' | '-' => {
                let found = old
                    .iter()
                    .enumerate()
                    .skip(cursor)
                    .find(|(_, line)| line.trim_end_matches(['\n', '\r']) == text)
                    .map(|(i, _)| i)
                    .with_context(|| {
                        format!("Update File {path}: context line not found: {text:?}")
                    })?;
                // Everything skipped before the match is unchanged context that
                // the patch did not name; keep it verbatim.
                for line in &old[cursor..found] {
                    out.push_str(line);
                }
                if *op == '-' {
                    cursor = found + 1;
                } else {
                    out.push_str(old[found]);
                    cursor = found + 1;
                }
            }
            '+' => {
                out.push_str(text);
                out.push('\n');
            }
            other => bail!("internal patch op {other:?}"),
        }
    }
    for line in &old[cursor..] {
        out.push_str(line);
    }
    Ok(out)
}

/// Execute the v2 `patch` tool. `input.patchText` is the envelope.
pub fn apply(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let text = input
        .get("patchText")
        .and_then(Value::as_str)
        .context("patch.patchText is required")?;
    let changes = parse(text)?;
    let mut summary: Vec<String> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    for change in changes {
        match change {
            Change::Add { path, content } => {
                refuse_git_dir(&path)?;
                // Existence is checked LEXICALLY first (a metadata read cannot
                // write anything), because `create_within` creates the leaf as
                // part of proving containment — checking after would see our
                // own new file.
                if cwd.join(&path).symlink_metadata().is_ok() {
                    bail!("Add File {path}: file already exists");
                }
                // Then create it through the kernel-verified primitive, which
                // builds missing parents one component at a time so
                // `*** Add File: new/dir/x.txt` works in-tree while a
                // symlinked component out of the tree is refused.
                use ocserve_store::containment::{OpenError, create_within};
                create_within(cwd, &path, content.as_bytes()).map_err(|e: OpenError| {
                    anyhow::anyhow!("Add File {path}: not inside the project: {e}")
                })?;
                summary.push(format!("A {path}"));
                files.push(json!({"path": path, "status": "added"}));
            }
            Change::Delete { path } => {
                refuse_git_dir(&path)?;
                let target = contained_path(cwd, &path)?;
                // A directory is never a valid patch target.
                if target.is_dir() {
                    bail!("Delete File {path}: is a directory");
                }
                remove_contained(&target)?;
                summary.push(format!("D {path}"));
                files.push(json!({"path": path, "status": "deleted"}));
            }
            Change::Update { path, move_to, ops } => {
                refuse_git_dir(&path)?;
                if let Some(to) = &move_to {
                    refuse_git_dir(to)?;
                }
                let target = contained_path(cwd, &path)?;
                let before = std::fs::read_to_string(&target)
                    .with_context(|| format!("read {}", target.display()))?;
                let after = apply_ops(&path, &before, &ops)?;
                let dest = match &move_to {
                    Some(to) => contained_path(cwd, to)?,
                    None => target.clone(),
                };
                write_contained(&dest, after.as_bytes())?;
                if dest != target {
                    remove_contained(&target)?;
                }
                let shown = move_to.unwrap_or(path);
                summary.push(format!("M {shown}"));
                files.push(json!({"path": shown, "status": "modified"}));
            }
        }
    }
    let output = format!(
        "Success. Updated the following files:\n{}",
        summary.join("\n")
    );
    Ok(ToolResult {
        output,
        truncated: false,
        exit: None,
        title: "patch".to_string(),
        error: false,
        metadata: Some(json!({"files": files})),
        output_path: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_update_move_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        apply(
            &json!({"patchText": "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** End Patch"}),
            cwd,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        apply(
            &json!({"patchText": "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@ x\n-one\n+ONE\n*** End Patch"}),
            cwd,
        )
        .unwrap();
        assert!(!cwd.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(cwd.join("b.txt")).unwrap(),
            "ONE\ntwo\n"
        );
        apply(
            &json!({"patchText": "*** Begin Patch\n*** Delete File: b.txt\n*** End Patch"}),
            cwd,
        )
        .unwrap();
        assert!(!cwd.join("b.txt").exists());
    }

    #[test]
    fn context_mismatch_never_guesses() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "actual\n").unwrap();
        let err = apply(
            &json!({"patchText": "*** Begin Patch\n*** Update File: a.txt\n-expected\n+new\n*** End Patch"}),
            tmp.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("context line not found"), "{err}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "actual\n"
        );
    }

    #[test]
    fn missing_envelope_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(apply(&json!({"patchText": "nope"}), tmp.path()).is_err());
    }
    /// Security review H1 (2026-11-26): the v2 `patch` tool wrote with
    /// `cwd.join(path)` and no containment, so `..` or an absolute path
    /// escaped the project. Each case below must be REFUSED, and the control
    /// proves an in-tree path still works.
    #[test]
    fn patch_refuses_to_escape_the_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("proj");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("in.txt"), "x\n").unwrap();

        // CONTROL: a legal in-tree add still works.
        let ok = apply(
            &json!({"patchText": "*** Begin Patch\n*** Add File: ok.txt\n+hi\n*** End Patch\n"}),
            &base,
        );
        assert!(ok.is_ok(), "in-tree add must succeed: {ok:?}");
        assert!(base.join("ok.txt").exists());

        for (label, path) in [
            ("dotdot", "../escape.txt"),
            ("nested dotdot", "sub/../../escape.txt"),
            ("absolute", "/tmp/ocserve-patch-escape.txt"),
            ("git hooks", ".git/hooks/pre-commit"),
        ] {
            let text = format!("*** Begin Patch\n*** Add File: {path}\n+pwned\n*** End Patch\n");
            let r = apply(&json!({ "patchText": text }), &base);
            assert!(r.is_err(), "{label}: patch must refuse {path:?}, got {r:?}");
        }
        // The absolute target must not exist as a side effect of the attempt.
        assert!(!std::path::Path::new("/tmp/ocserve-patch-escape.txt").exists());

        // Update + Move to must be contained too.
        let mv = "*** Begin Patch\n*** Update File: in.txt\n*** Move to: ../moved.txt\n-x\n+y\n*** End Patch\n";
        assert!(apply(&json!({ "patchText": mv }), &base).is_err());
        assert!(!dir.path().join("moved.txt").exists());
    }
}
