//! Shell discovery for `GET /pty/shells` — freeze parity with
//! core/src/shell.ts:
//! - read `/etc/shells` (comments/blank lines skipped), dedup by full path
//!   preserving order; when the file is missing/empty, fall back to
//!   `["/bin/bash", "/bin/zsh", "/bin/sh"]`.
//! - each item: {path, name: basename lowercased, acceptable: not in the
//!   deny-list}. Deny-list = fish, nu (META table: `deny: true`).
//! - `login(shell)` = bash/dash/fish/ksh/sh/zsh (META `login: true`).
//! - On Windows there is no `/etc/shells`; upstream's `win()` builds the list
//!   from `pwsh`, `powershell`, git-bash and `%COMSPEC%` (core/src/shell.ts).

use serde_json::{Value, json};

/// Denied shells (META deny:true in core/src/shell.ts).
pub fn acceptable(path: &str) -> bool {
    !matches!(name(path).as_str(), "fish" | "nu")
}

pub fn name(path: &str) -> String {
    // Split on both separators: Windows shells come as `C:\...\pwsh.exe`, and
    // `name()` feeds the deny-list (`fish`, `nu`) and the login-appending rule.
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
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
///
/// Windows has no `/etc/shells`, so it uses upstream's `win()` order:
/// `pwsh`, `powershell`, git-bash, `%COMSPEC%` (cmd.exe). Returning POSIX paths
/// there was a real bug found by the Windows VM: the API advertised
/// `/bin/bash`, `/bin/zsh`, `/bin/sh`, none of which exist.
fn raw_shells() -> Vec<String> {
    #[cfg(windows)]
    {
        return windows_shells();
    }
    #[cfg(not(windows))]
    {
        unix_shells()
    }
}

/// Resolve an executable on PATH (suffix already included by the caller).
#[cfg(windows)]
fn which_exe(prog: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(prog);
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(windows)]
fn windows_shells() -> Vec<String> {
    use which_exe;
    let mut out: Vec<String> = Vec::new();
    // PowerShell 7 first (upstream prefers it), then Windows PowerShell 5.1,
    // then git-bash, then the mandatory COMSPEC.
    let mut push = |p: Option<String>| {
        if let Some(p) = p
            && !out.iter().any(|e| e.eq_ignore_ascii_case(&p))
        {
            out.push(p);
        }
    };
    push(which_exe("pwsh.exe"));
    push(which_exe("powershell.exe"));
    // git-bash lives beside git: <git>/../../bin/bash.exe
    if let Some(git) = which_exe("git.exe") {
        let p = std::path::Path::new(&git)
            .parent()
            .and_then(|d| d.parent())
            .map(|d| d.join("bin").join("bash.exe"));
        if let Some(p) = p.filter(|p| p.is_file()) {
            push(Some(p.to_string_lossy().into_owned()));
        }
    }
    push(
        std::env::var("COMSPEC")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| which_exe("cmd.exe")),
    );
    if out.is_empty() {
        out.push("cmd.exe".to_string());
    }
    out
}

#[cfg(not(windows))]
fn unix_shells() -> Vec<String> {
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
    #[cfg(windows)]
    {
        // First acceptable entry of the Windows list: pwsh, powershell, git
        // bash, COMSPEC. Never a POSIX path, which does not exist there.
        return windows_shells()
            .into_iter()
            .find(|p| acceptable(p))
            .unwrap_or_else(|| "cmd.exe".to_string());
    }
    #[cfg(not(windows))]
    {
        let bash = "/bin/bash";
        if std::path::Path::new(bash).is_file() {
            return bash.to_string();
        }
        "/bin/sh".to_string()
    }
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

    /// Windows must advertise Windows shells, never POSIX paths.
    ///
    /// CONTROL: the first version returned `/bin/bash`, `/bin/zsh`, `/bin/sh`
    /// on Windows — paths that do not exist — because `raw_shells` only knew
    /// `/etc/shells`. Found by the Windows VM, not by any test.
    #[cfg(windows)]
    #[test]
    fn windows_lists_windows_shells() {
        let l = list();
        assert!(!l.is_empty(), "Windows must always find at least cmd.exe");
        for item in &l {
            let p = item["path"].as_str().unwrap();
            assert!(
                !p.starts_with("/bin/"),
                "Windows advertised a POSIX path: {p}"
            );
            assert!(
                p.to_ascii_lowercase().ends_with(".exe"),
                "Windows shell should be an executable path: {p}"
            );
        }
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
