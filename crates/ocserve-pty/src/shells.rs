//! Shell discovery for `GET /pty/shells` — freeze parity with
//! core/src/shell.ts:
//! - read `/etc/shells` (comments/blank lines skipped), dedup by full path
//!   preserving order; when the file is missing/empty, fall back to
//!   `["/bin/bash", "/bin/zsh", "/bin/sh"]`.
//! - each item: {path, name: basename lowercased, acceptable: not in the
//!   deny-list}. Deny-list = fish, nu (META table: `deny: true`).
//! - `login(shell)` = bash/dash/fish/ksh/sh/zsh (META `login: true`).

use serde_json::{Value, json};

/// Denied shells (META deny:true in core/src/shell.ts).
pub fn acceptable(path: &str) -> bool {
    !matches!(name(path).as_str(), "fish" | "nu")
}

pub fn name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

/// META login:true — create() appends "-l".
pub fn login(path: &str) -> bool {
    matches!(
        name(path).as_str(),
        "bash" | "dash" | "fish" | "ksh" | "sh" | "zsh"
    )
}

/// Raw /etc/shells entries (dedup, comments/blank skipped); fallback list
/// when unreadable/empty.
fn raw_shells() -> Vec<String> {
    let text = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !out.iter().any(|s| s == t) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        out = vec!["/bin/bash".into(), "/bin/zsh".into(), "/bin/sh".into()];
    }
    out
}

pub fn list() -> Vec<Value> {
    raw_shells()
        .into_iter()
        .map(|path| {
            json!({
                "path": path,
                "name": name(&path),
                "acceptable": acceptable(&path),
            })
        })
        .collect()
}

/// Preferred shell: `$SHELL` when it resolves, else bash, else /bin/sh
/// (core/src/shell.ts fallback()).
pub fn preferred(cwd_root: &str) -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
        && std::path::Path::new(&shell).is_file()
        && acceptable(&shell)
    {
        return shell;
    }
    let _ = cwd_root;
    let bash = "/bin/bash";
    if std::path::Path::new(bash).is_file() {
        return bash.to_string();
    }
    "/bin/sh".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_acceptability() {
        assert_eq!(name("/usr/bin/bash"), "bash");
        assert_eq!(name("ZSH"), "zsh");
        assert!(acceptable("/bin/bash"));
        assert!(!acceptable("/usr/bin/fish"));
        assert!(!acceptable("/usr/bin/nu"));
        assert!(login("/bin/zsh"));
        assert!(login("/usr/bin/fish"));
        assert!(!login("/usr/bin/git-shell"));
        assert!(!login("/bin/rbash"));
    }

    #[test]
    fn list_has_shape_and_dedup() {
        let l = list();
        assert!(!l.is_empty());
        for item in &l {
            assert!(item["path"].is_string());
            assert!(item["name"].is_string());
            assert!(item["acceptable"].is_boolean());
            assert_eq!(
                item["name"].as_str().unwrap(),
                name(item["path"].as_str().unwrap())
            );
        }
        // dedup check: no duplicate paths
        let mut paths: Vec<&str> = l.iter().map(|v| v["path"].as_str().unwrap()).collect();
        let n = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), n, "duplicate shell paths in /pty/shells");
    }
}
