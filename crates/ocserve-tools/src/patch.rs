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

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
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
                let target = resolve(cwd, &path);
                if target.exists() {
                    bail!("Add File {path}: file already exists");
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                std::fs::write(&target, content.as_bytes())
                    .with_context(|| format!("write {}", target.display()))?;
                summary.push(format!("A {path}"));
                files.push(json!({"path": path, "status": "added"}));
            }
            Change::Delete { path } => {
                let target = resolve(cwd, &path);
                std::fs::remove_file(&target)
                    .with_context(|| format!("delete {}", target.display()))?;
                summary.push(format!("D {path}"));
                files.push(json!({"path": path, "status": "deleted"}));
            }
            Change::Update { path, move_to, ops } => {
                let target = resolve(cwd, &path);
                let before = std::fs::read_to_string(&target)
                    .with_context(|| format!("read {}", target.display()))?;
                let after = apply_ops(&path, &before, &ops)?;
                let dest = match &move_to {
                    Some(to) => resolve(cwd, to),
                    None => target.clone(),
                };
                std::fs::write(&dest, after.as_bytes())
                    .with_context(|| format!("write {}", dest.display()))?;
                if dest != target {
                    std::fs::remove_file(&target)
                        .with_context(|| format!("remove {}", target.display()))?;
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
}
