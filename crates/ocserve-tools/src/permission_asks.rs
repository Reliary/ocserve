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
use serde_json::Value;
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
        "webfetch" => single_always("webfetch", args),
        "websearch" => single_always("websearch", args),
        "todowrite" => single_always("todowrite", args),
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

fn single(permission: &str, pattern: String, args: &Value, cwd: &Path) -> Vec<Ask> {
    let mut asks = Vec::new();
    if let Some(a) = external_dir_ask(cwd, &pattern) {
        asks.push(a);
    }
    asks.push(Ask {
        permission: permission.to_string(),
        patterns: vec![pattern],
        always: vec!["*".to_string()],
        metadata: args.clone(),
    });
    asks
}

fn single_always(permission: &str, args: &Value) -> Vec<Ask> {
    vec![Ask {
        permission: permission.to_string(),
        patterns: vec!["*".to_string()],
        always: vec!["*".to_string()],
        metadata: args.clone(),
    }]
}

/// read/edit/write: check the path against the worktree, then ask the tool's
/// permission with the worktree-relative path (upstream paths are relative).
fn fs_asks(permission: &str, args: &Value, cwd: &Path, key: &str) -> Vec<Ask> {
    let raw = args.get(key).and_then(|v| v.as_str()).unwrap_or("*");
    let mut asks = Vec::new();
    if let Some(a) = external_dir_ask(cwd, raw) {
        asks.push(a);
    }
    // relative to worktree when possible (upstream path.relative)
    let rel = relative_to(cwd, raw);
    asks.push(Ask {
        permission: permission.to_string(),
        patterns: vec![rel],
        always: vec!["*".to_string()],
        metadata: args.clone(),
    });
    asks
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
    fn glob_single_pattern() {
        let asks = asks_for("glob", &json!({"pattern": "**/*.rs"}), &cwd());
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "glob");
        assert_eq!(asks[0].patterns, vec!["**/*.rs"]);
    }
}
