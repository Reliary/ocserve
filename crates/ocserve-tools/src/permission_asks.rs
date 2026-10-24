//! Tool → permission ask sequence (upstream tool-call `ctx.ask()` parity).
//!
//! Upstream tools do not perform a single permission check: each tool calls
//! `ctx.ask()` zero or more times before executing, in order. Examples:
//!   - read/edit/write: an `external_directory` ask when the path is outside
//!     the worktree, then the tool's own ask (`read` / `edit`).
//!   - write/apply_patch ask with permission `"edit"` (the edit/read alias).
//!   - bash: `external_directory` asks for referenced dirs, then a `bash` ask
//!     whose patterns are the per-subcommand strings and whose `always` grants
//!     are arity prefixes (`"git status *"`).
//!   - glob/grep: a single ask with the pattern.
//!
//! ocserve previously derived one resource string per call and asked once,
//! which diverged in four ways (write not aliased to edit; bash always =
//! whole command; no external_directory gate; compound commands as one
//! resource). This module produces the exact ordered ask list so the prompt
//! loop evaluates each like upstream.

use crate::bash_patterns;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// One `ctx.ask()` request: `permission` names the rule family; `patterns`
/// are the concrete resources; `always` are the patterns an "always" reply
/// should whitelist; `metadata` is surfaced to the client.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub permission: String,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    pub metadata: Value,
}

/// Path-taking commands (upstream shell.ts FILES): their non-flag args are
/// candidate external directories.
const PATH_COMMANDS: &[&str] = &[
    "cd",
    "chdir",
    "popd",
    "pushd",
    "rm",
    "cp",
    "mv",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "cat",
    "get-content",
    "set-content",
    "add-content",
    "copy-item",
    "move-item",
    "remove-item",
    "new-item",
    "rename-item",
];

/// Build the ordered ask sequence for a tool call. `cwd` is the worktree root;
/// `args` is the parsed tool input.
pub fn asks_for(tool: &str, args: &Value, cwd: &Path) -> Vec<Ask> {
    match tool {
        "bash" => bash_asks(args, cwd),
        "read" => fs_asks("read", args, cwd, "filePath"),
        "write" | "apply_patch" => fs_asks("edit", args, cwd, "filePath"),
        "edit" => fs_asks("edit", args, cwd, "filePath"),
        "glob" => single("glob", str_arg(args, "pattern"), args, cwd),
        "grep" => single("grep", str_arg(args, "pattern"), args, cwd),
        "webfetch" => webfetch_ask(args),
        // v1 skill tool: ctx.ask({permission:"skill", patterns:[name],
        // always:[name]}) — pattern is the skill name, not "*".
        "skill" => vec![Ask {
            permission: "skill".to_string(),
            patterns: vec![str_arg(args, "name")],
            always: vec![str_arg(args, "name")],
            metadata: Value::Null,
        }],
        "websearch" => single_always("websearch"),
        "todowrite" => single_always("todowrite"),
        // v1 task tool: ctx.ask({permission:"task", patterns:[subagent_type],
        // always:["*"]}) — pattern is the subagent type, not "*".
        "task" => vec![Ask {
            permission: "task".to_string(),
            patterns: vec![str_arg(args, "subagent_type")],
            always: vec!["*".to_string()],
            metadata: json!({
                "description": args.get("description").cloned().unwrap_or(Value::Null),
                "subagent_type": args.get("subagent_type").cloned().unwrap_or(Value::Null),
            }),
        }],
        // Unknown/other tools: a wildcard ask (upstream default = ask).
        _ => vec![Ask {
            permission: tool.to_string(),
            patterns: vec!["*".to_string()],
            always: vec!["*".to_string()],
            metadata: Value::Null,
        }],
    }
}

fn str_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("*")
        .to_string()
}

/// `glob` / `grep`: upstream metadata is the tool's own named fields, and both
/// emit `{pattern, path?, include?}` — NOT the raw arguments object. Sending
/// the raw args leaked the caller's keys (measured: ours had
/// `{"path":null,"pattern":"workspace"}`, reference had the same three keys
/// with nulls omitted). Ask metadata is client-visible, so it is wire.
fn single(permission: &str, pattern: String, args: &Value, cwd: &Path) -> Vec<Ask> {
    let mut asks = Vec::new();
    if let Some(a) = external_dir_ask(cwd, &pattern) {
        asks.push(a);
    }
    // Upstream builds these objects with `path: params.path` where the value
    // is `undefined` when absent, and JSON.stringify DROPS undefined keys. So
    // the wire carries only the keys the model actually supplied (measured:
    // reference `{pattern}` vs ours `{path:null,pattern}`). Absent keys must be
    // omitted, not nulled.
    let metadata = match permission {
        "glob" => present(&[
            ("pattern", Some(json!(pattern))),
            ("path", args.get("path").cloned()),
        ]),
        "grep" => present(&[
            ("pattern", Some(json!(pattern))),
            ("path", args.get("path").cloned()),
            ("include", args.get("include").cloned()),
        ]),
        _ => serde_json::json!({}),
    };
    asks.push(Ask {
        permission: permission.to_string(),
        patterns: vec![pattern],
        always: vec!["*".to_string()],
        metadata,
    });
    asks
}

/// Build a JSON object the way `JSON.stringify` does: keys whose value is
/// absent (`undefined` upstream) are omitted entirely rather than sent as null.
fn present(pairs: &[(&str, Option<Value>)]) -> Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            obj.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(obj)
}

/// `todowrite` / `websearch`: a fixed `*` pattern and EMPTY metadata upstream
/// (`todo.ts:27`, `websearch.ts:122`). `webfetch` is different and is built
/// separately below.
fn single_always(permission: &str) -> Vec<Ask> {
    vec![Ask {
        permission: permission.to_string(),
        patterns: vec!["*".to_string()],
        always: vec!["*".to_string()],
        metadata: serde_json::json!({}),
    }]
}

/// `webfetch` (`webfetch.ts:42`): pattern is the URL itself, always `*`, and
/// the metadata carries url/format/timeout.
fn webfetch_ask(args: &Value) -> Vec<Ask> {
    vec![Ask {
        permission: "webfetch".to_string(),
        patterns: vec![str_arg(args, "url")],
        always: vec!["*".to_string()],
        // `format` has a decoding default upstream (webfetch.ts:16), so the
        // ask metadata always carries it even when the model omitted it.
        // `timeout` is genuinely optional and omitted when absent.
        metadata: present(&[
            ("url", args.get("url").cloned()),
            (
                "format",
                Some(
                    args.get("format")
                        .cloned()
                        .unwrap_or_else(|| json!("markdown")),
                ),
            ),
            ("timeout", args.get("timeout").cloned()),
        ]),
    }]
}

/// read/edit/write: check the path against the worktree, then ask the tool's
/// permission with the worktree-relative path (upstream paths are relative).
///
/// Metadata is tool-specific and client-visible, so it is part of the wire
/// contract, not an implementation detail (measured divergence 2026-10-24):
///   - `read`   → `{}` (read.ts:257)
///   - `edit`   → `{filepath, diff}` (edit.ts:106)
///   - `write`  → `{filepath, diff}` (write.ts:57)
///
/// ocserve previously forwarded the raw model arguments, which meant clients
/// saw `oldString`/`content` where the reference sends a rendered diff.
fn fs_asks(permission: &str, args: &Value, cwd: &Path, key: &str) -> Vec<Ask> {
    let raw = args.get(key).and_then(|v| v.as_str()).unwrap_or("*");
    let mut asks = Vec::new();
    if let Some(a) = external_dir_ask(cwd, raw) {
        asks.push(a);
    }
    // relative to worktree when possible (upstream path.relative)
    let rel = relative_to(cwd, raw);
    let metadata = match permission {
        "read" => serde_json::json!({}),
        "edit" => serde_json::json!({
            "filepath": raw,
            "diff": ask_diff(args, cwd),
        }),
        _ => serde_json::json!({}),
    };
    asks.push(Ask {
        permission: permission.to_string(),
        patterns: vec![rel],
        always: vec!["*".to_string()],
        metadata,
    });
    asks
}

/// The `diff` the reference includes in edit/write ask metadata.
///
/// Upstream renders a full two-file patch with the `diff` npm package and then
/// trims shared indentation (`edit.ts:646 trimDiff`, min-indent across content
/// lines). We render the same header, hunk arithmetic and content with
/// `similar`'s line diff and port trimDiff verbatim, so the text matches for
/// every case the oracle covers; the one structural difference is that
/// upstream's Myers output can choose a different equally-valid alignment than
/// `similar`'s, which is recorded as D-PERM-ASK-DIFF and compared as a key set
/// by the oracle rather than byte-equality.
fn render_patch(path: &str, old: &str, new: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    const CONTEXT: usize = 3;
    let diff = TextDiff::from_lines(old, new);
    let mut body = String::new();
    for group in diff.grouped_ops(CONTEXT) {
        let first = group.first().expect("group is non-empty");
        let old_range = first.old_range();
        let new_range = first.new_range();
        // Unified-diff line numbering: a NON-EMPTY range starts at
        // start+1, an EMPTY range names the line before it (so a pure
        // insertion at the top is "@@ -0,0 +1,N @@" — the reference's
        // exact output for the create-via-edit path).
        let old_start = if old_range.is_empty() {
            old_range.start
        } else {
            old_range.start + 1
        };
        let new_start = if new_range.is_empty() {
            new_range.start
        } else {
            new_range.start + 1
        };
        body.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start,
            old_range.len(),
            new_start,
            new_range.len()
        ));
        for op in &group {
            for change in diff.iter_changes(op) {
                body.push(match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                });
                body.push_str(change.value());
                if change.missing_newline() {
                    body.push('\n');
                    body.push_str("\\ No newline at end of file\n");
                }
            }
        }
    }
    let untrimmed = format!(
        "Index: {path}\n===================================================================\n--- {path}\n+++ {path}\n{body}"
    );
    trim_diff(&untrimmed)
}

/// Upstream `trimDiff` (edit.ts:646): strip the smallest shared indentation
/// from every content line so a nested edit does not render as a wall of
/// leading spaces. Header and hunk lines are left untouched.
fn trim_diff(diff: &str) -> String {
    let mut min: Option<usize> = None;
    for line in diff.split('\n') {
        let is_content = (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
            && !line.starts_with("---")
            && !line.starts_with("+++");
        if !is_content {
            continue;
        }
        let content = &line[1..];
        if content.trim().is_empty() {
            continue;
        }
        let indent = content.len() - content.trim_start().len();
        min = Some(min.map_or(indent, |m: usize| m.min(indent)));
    }
    let Some(min) = min else {
        return diff.to_string();
    };
    if min == 0 {
        return diff.to_string();
    }
    let mut out = String::with_capacity(diff.len());
    for (i, line) in diff.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let is_content = (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
            && !line.starts_with("---")
            && !line.starts_with("+++");
        if is_content && line.len() > 1 + min {
            out.push_str(&line[..1]);
            out.push_str(&line[1 + min..]);
        } else if is_content {
            out.push_str(&line[..1]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// `write` passes `content` and no `oldString`; `edit` passes `oldString`.
fn permission_is_write(args: &Value) -> bool {
    args.get("oldString").is_none() && args.get("content").is_some()
}

/// The diff an edit/write ask carries: current file content vs the intended
/// result, exactly what the reference computes before it asks.
fn ask_diff(args: &Value, cwd: &Path) -> String {
    let fp = args.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
    let full = resolve(cwd, fp);
    let path = full.to_string_lossy().to_string();
    let current = std::fs::read_to_string(&full).unwrap_or_default();
    let updated = if permission_is_write(args) {
        args.get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        let old = args
            .get("oldString")
            .or_else(|| args.get("oldText"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new = args
            .get("newString")
            .or_else(|| args.get("newText"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if old.is_empty() {
            new.to_string()
        } else if args
            .get("replaceAll")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            current.replace(old, new)
        } else {
            current.replacen(old, new, 1)
        }
    };
    render_patch(&path, &current, &updated)
}

/// `external_directory` ask when `target` resolves outside the worktree.
/// Mirrors upstream external-directory.ts: dir = target (if dir) else parent;
/// glob = dir + "/*", both absolute-ish strings.
fn external_dir_ask(cwd: &Path, target: &str) -> Option<Ask> {
    let full = resolve(cwd, target);
    if contains(cwd, &full) {
        return None;
    }
    // treat as a file unless it is an existing directory
    let dir = if full.is_dir() {
        full.clone()
    } else {
        full.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| full.clone())
    };
    let glob = format!("{}/*", dir.to_string_lossy());
    Some(Ask {
        permission: "external_directory".to_string(),
        patterns: vec![glob.clone()],
        always: vec![glob],
        metadata: serde_json::json!({"filepath": full.to_string_lossy(), "parentDir": dir.to_string_lossy()}),
    })
}

fn bash_asks(args: &Value, cwd: &Path) -> Vec<Ask> {
    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let scan = bash_patterns::scan(command);
    let mut asks = Vec::new();
    // external_directory for path-taking subcommands (upstream shell.scan.dirs)
    let mut dirs: Vec<String> = Vec::new();
    for seg in bash_patterns::split_commands(command) {
        let tokens = bash_patterns::tokenize(&seg);
        let Some(cmd) = tokens.first() else { continue };
        if !PATH_COMMANDS.contains(&cmd.as_str()) {
            continue;
        }
        for t in tokens.iter().skip(1) {
            if t.starts_with('-') {
                continue;
            }
            let full = resolve(cwd, t);
            if !contains(cwd, &full) {
                let dir = if full.is_dir() {
                    full
                } else {
                    full.parent().map(Path::to_path_buf).unwrap_or(full)
                };
                dirs.push(format!("{}/*", dir.to_string_lossy()));
            }
        }
    }
    if !dirs.is_empty() {
        asks.push(Ask {
            permission: "external_directory".to_string(),
            patterns: dirs.clone(),
            always: dirs.clone(),
            metadata: serde_json::json!({"command": command, "directories": dirs, "patterns": dirs}),
        });
    }
    if scan.patterns.is_empty() {
        return asks;
    }
    asks.push(Ask {
        permission: "bash".to_string(),
        patterns: scan.patterns,
        always: scan.always,
        metadata: serde_json::json!({"command": command}),
    });
    asks
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Worktree-relative path (upstream `path.relative(worktree, filepath)`); the
/// raw string when it cannot be made relative.
fn relative_to(cwd: &Path, p: &str) -> String {
    let full = resolve(cwd, p);
    match full.strip_prefix(cwd) {
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => p.to_string(),
    }
}

/// Whether `full` is inside `cwd` (lexical containment; upstream containsPath).
fn contains(cwd: &Path, full: &Path) -> bool {
    // normalize both lexically (no fs access — a nonexistent path still counts
    // as "inside" when its lexical location is under cwd)
    let c = normalize(cwd);
    let f = normalize(full);
    f.starts_with(&c)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cwd() -> PathBuf {
        PathBuf::from("/work/proj")
    }

    #[test]
    fn write_aliases_to_edit() {
        let asks = asks_for("write", &json!({"filePath": "src/x.rs"}), &cwd());
        assert_eq!(asks.last().unwrap().permission, "edit");
        assert_eq!(asks.last().unwrap().patterns, vec!["src/x.rs"]);
    }

    #[test]
    fn edit_relative_path() {
        let asks = asks_for("edit", &json!({"filePath": "src/x.rs"}), &cwd());
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "edit");
    }

    #[test]
    fn external_file_prepends_external_dir_ask() {
        let asks = asks_for("read", &json!({"filePath": "/etc/passwd"}), &cwd());
        assert_eq!(asks.len(), 2, "external_directory then read");
        assert_eq!(asks[0].permission, "external_directory");
        assert_eq!(asks[0].patterns, vec!["/etc/*"]);
        assert_eq!(asks[1].permission, "read");
    }

    #[test]
    fn internal_file_no_external_ask() {
        let asks = asks_for("read", &json!({"filePath": "src/x.rs"}), &cwd());
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "read");
    }

    #[test]
    fn bash_compound_patterns_and_arity_always() {
        let asks = asks_for(
            "bash",
            &json!({"command": "git status && rm -rf /tmp/x"}),
            &cwd(),
        );
        let bash = asks.iter().find(|a| a.permission == "bash").unwrap();
        assert_eq!(bash.patterns.len(), 2);
        assert!(bash.always.contains(&"git status *".to_string()));
        assert!(bash.always.contains(&"rm *".to_string()));
    }

    #[test]
    fn bash_external_dir_for_out_of_tree() {
        let asks = asks_for("bash", &json!({"command": "cat /etc/hosts"}), &cwd());
        assert_eq!(asks[0].permission, "external_directory");
        assert_eq!(asks[0].patterns, vec!["/etc/*"]);
    }

    #[test]
    fn read_ask_metadata_is_empty_object() {
        // Upstream read.ts:257 sends `metadata: {}`; forwarding the raw
        // arguments (the pre-2026-10-24 behaviour) leaked `filePath` to the
        // client and diverged from the reference.
        let asks = asks_for("read", &json!({"filePath": "src/x.rs"}), &cwd());
        assert_eq!(asks[0].metadata, json!({}));
    }

    #[test]
    fn todowrite_ask_metadata_is_empty_object() {
        let asks = asks_for("todowrite", &json!({"todos": []}), &cwd());
        assert_eq!(asks[0].permission, "todowrite");
        assert_eq!(asks[0].patterns, vec!["*"]);
        assert_eq!(asks[0].metadata, json!({}));
    }

    #[test]
    fn webfetch_pattern_is_the_url() {
        // Upstream webfetch.ts:42 asks with the URL as the pattern, not `*`.
        let asks = asks_for(
            "webfetch",
            &json!({"url": "https://example.com", "format": "markdown"}),
            &cwd(),
        );
        assert_eq!(asks[0].patterns, vec!["https://example.com"]);
        let meta = asks[0].metadata.as_object().unwrap();
        assert!(meta.contains_key("url"));
        assert!(meta.contains_key("format"));
        // `format` is defaulted upstream, `timeout` is genuinely optional and
        // must be OMITTED (JSON.stringify drops undefined) rather than nulled.
        assert_eq!(
            meta.get("format").and_then(|v| v.as_str()),
            Some("markdown")
        );
        assert!(
            !meta.contains_key("timeout"),
            "absent optional key must be omitted, got {meta:?}"
        );
    }

    #[test]
    fn glob_metadata_omits_absent_path() {
        // Measured reference: `{pattern}` only. Sending `path: null` diverged.
        let asks = asks_for("glob", &json!({"pattern": "**/*.rs"}), &cwd());
        let meta = asks[0].metadata.as_object().unwrap();
        assert_eq!(meta.len(), 1, "only pattern, got {meta:?}");
        assert!(meta.contains_key("pattern"));
    }

    #[test]
    fn write_ask_carries_filepath_and_diff() {
        let asks = asks_for(
            "write",
            &json!({"filePath": "src/new.rs", "content": "hello\n"}),
            &cwd(),
        );
        let meta = asks[0].metadata.as_object().unwrap();
        assert!(meta.contains_key("filepath"));
        assert!(meta.contains_key("diff"));
    }

    #[test]
    fn create_via_empty_oldstring_renders_add_hunk() {
        // The reference's create path (edit.ts:88) produces a
        // "@@ -0,0 +1,N @@" add-only hunk. This pins the hunk arithmetic.
        let asks = asks_for(
            "edit",
            &json!({"filePath": "src/new.rs", "oldString": "", "newString": "hello\nworld\n"}),
            &cwd(),
        );
        let diff = asks[0].metadata["diff"].as_str().unwrap();
        assert!(diff.contains("@@ -0,0 +1,2 @@"), "diff was: {diff}");
        assert!(diff.contains("+hello"));
    }

    #[test]
    fn trim_diff_strips_shared_indent() {
        let untrimmed = "@@ -1,1 +1,1 @@\n     alpha\n-    beta\n+    BETA\n";
        let trimmed = trim_diff(untrimmed);
        assert!(
            trimmed.contains("\n alpha\n-beta\n+BETA\n"),
            "got: {trimmed}"
        );
    }

    #[test]
    fn glob_single_pattern() {
        let asks = asks_for("glob", &json!({"pattern": "**/*.rs"}), &cwd());
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "glob");
        assert_eq!(asks[0].patterns, vec!["**/*.rs"]);
    }
}
