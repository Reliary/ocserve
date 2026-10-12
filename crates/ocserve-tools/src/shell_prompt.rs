//! The `bash` tool description is TEMPLATED upstream (tool/shell/prompt.ts
//! `ShellPrompt.render`): `${os}`, `${shell}`, `${tmp}`, and a generated
//! command section are substituted at runtime. A capture therefore bakes in
//! the capturing host's values — never ship that. This mirrors the upstream
//! template so the description is correct on the running host (K-CAPDIFF:
//! capdiff compares description bytes too; a stale/host-baked string is red).
//!
//! The unix/bash profile is implemented (ocserve ships on unix); the
//! powershell/cmd profiles are a named gap (D-TOOLS-SHELL-WINDOWS).

const SHELL_TEMPLATE: &str = include_str!("../assets/tool/shell.txt");
const COMMAND_SECTION: &str = include_str!("../assets/tool/bash-command-section.txt");

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Upstream `chainGuidance` for the bash (posix) profile.
const CHAIN_BASH: &str = "If the commands depend on each other and must run sequentially, use a single Bash call with '&&' to chain them together (e.g., `git add . && git commit -m \"message\" && git push`). For instance, if one operation must complete before another starts (like mkdir before cp, Write before Bash for git operations, or git add before git commit), run these operations sequentially instead.";

/// Render the bash tool description for the running host. `shell` is the
/// user's shell basename (upstream `Shell.name(Shell.acceptable(cfg.shell))`);
/// `tmp` is the model temp dir (upstream `Global.Path.tmp`).
pub fn bash_description(shell: &str, tmp: &str) -> String {
    let command_section = render(COMMAND_SECTION, &[
        ("defaultTimeoutMs", &DEFAULT_TIMEOUT_MS.to_string()),
        ("limits.maxLines", &MAX_LINES.to_string()),
        ("limits.maxBytes", &MAX_BYTES.to_string()),
        ("chain", CHAIN_BASH),
    ]);
    render(SHELL_TEMPLATE, &[
        ("intro", "Executes a given bash command in a persistent shell session with optional timeout, ensuring proper handling and security measures."),
        ("os", std::env::consts::OS),
        ("shell", shell),
        ("workdirSection", "All commands run in the current working directory by default. Use the `workdir` parameter if you need to run a command in a different directory. AVOID using `cd <directory> && <command>` patterns - use `workdir` instead."),
        ("tmp", tmp),
        ("commandSection", &command_section),
    ])
}

/// Minimal `${key}` substitution (upstream `renderPrompt`). Unknown keys are
/// left literal.
fn render(tpl: &str, kv: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(tpl.len() + 256);
    let mut rest = tpl;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(close) => {
                let key = &rest[i + 2..i + 2 + close];
                match kv.iter().find(|(k, _)| *k == key) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push_str("${");
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &rest[i + 2 + close + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_description_fills_host_values() {
        let d = bash_description("zsh", "/tmp/opencode");
        assert!(d.starts_with("Executes a given bash command in a persistent shell session"));
        assert!(d.contains(&format!("Be aware: OS: {}, Shell: zsh", std::env::consts::OS)));
        assert!(d.contains("Use `/tmp/opencode` for temporary work"));
        assert!(d.contains("commands will time out after 120000ms"));
        assert!(d.contains("exceeds 2000 lines or 51200 bytes"));
        assert!(d.contains("\n   - If the command will create new directories"));
        assert!(!d.contains("${"));
    }
}
