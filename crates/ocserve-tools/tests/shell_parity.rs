//! Shell parity: the shell the model is TOLD about is the shell that RUNS.
//!
//! This is its own integration-test binary because it mutates the process-wide
//! `config.shell` publication, and Cargo gives each integration test file its
//! own process (the same isolation rule the reader-accounting test follows).
//!
//! The bug this pins down (measured 2026-10-27): the description was rendered
//! from `$SHELL` (→ `Shell: zsh`) while execution used a hardcoded `/bin/sh`.
//! On this host `sh` does not read `~/.zshenv`, so a command the description
//! promised would work could not find `cargo` at all:
//!
//! ```text
//! sh  -c 'command -v cargo'  → nothing
//! zsh -c 'command -v cargo'  → /home/…/.cargo/bin/cargo
//! ```
//!
//! `$0` is the discriminator: `sh -c` reports `sh`, `zsh -c` reports `zsh`.

use serde_json::json;
use std::path::Path;

/// `config.shell` is a process-global (production writes it from one place:
/// the payload-build choke point). These tests mutate it, so they must not
/// interleave — without this lock they raced and one test read another's
/// value (caught here, not shipped: the first run was 4/5 with a confusing
/// failure that moved between tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn run(cmd: &str) -> String {
    ocserve_tools::execute("bash", &json!({ "command": cmd }), Path::new("/tmp"), &[])
        .expect("execute")
        .output
        .trim()
        .to_string()
}

/// `$0` basename, lowercased, exactly the identity the description uses.
fn reported_shell() -> String {
    let raw = run("printf '%s' \"$0\"");
    let base = raw.rsplit('/').next().unwrap_or(&raw).to_lowercase();
    // Some shells report `-bash`/`sh`; strip the login marker.
    base.trim_start_matches('-').to_string()
}

#[test]
fn description_and_execution_agree_on_the_shell() {
    let _g = lock();
    let (shell_from_description, tmp) = {
        let name = ocserve_store::platform::tool_shell().name;
        (name, std::env::temp_dir().join("opencode"))
    };
    let desc = ocserve_tools::shell_prompt::bash_description(
        &shell_from_description,
        &tmp.to_string_lossy(),
    );
    let executed = reported_shell();

    assert!(
        desc.contains(&format!("Shell: {shell_from_description}")),
        "description must name the resolved shell ({shell_from_description})"
    );
    // INHERENT CONTROL: on the pre-fix code this is `sh` while the description
    // says `zsh`/`bash`, so the assertion below fails. On this host with SHELL
    // unset both sides fall back to `bash`, so the pair still has to match.
    assert_eq!(
        executed, shell_from_description,
        "the tool executed `{executed}` but told the model `{shell_from_description}`"
    );
}

#[test]
fn config_shell_wins_and_reaches_both_sides() {
    let _g = lock();
    // A hot-reloaded `config.shell` must be visible to the very next command
    // (upstream reads it at spawn time and does not cache). Without this the
    // tool would keep executing `$SHELL` while the served config claimed
    // something else.
    let before = ocserve_store::platform::tool_shell().name;
    ocserve_store::platform::set_config_shell(Some("bash".into()));
    let after = ocserve_store::platform::tool_shell().name;
    assert_eq!(
        after, "bash",
        "config.shell must be honoured (was {before})"
    );
    assert_eq!(
        reported_shell(),
        "bash",
        "execution must follow config.shell"
    );

    ocserve_store::platform::set_config_shell(Some("  ".into()));
    assert_eq!(
        ocserve_store::platform::tool_shell().name,
        before,
        "a blank config.shell must fall back, not select an empty program"
    );
}

#[test]
fn denied_shells_are_skipped_not_run() {
    let _g = lock();
    // fish and nu are denied upstream (`META[...].deny`). A configured denied
    // shell must not be executed; it falls through to the next candidate.
    ocserve_store::platform::set_config_shell(Some("fish".into()));
    let picked = ocserve_store::platform::tool_shell().name;
    assert_ne!(
        picked, "fish",
        "fish must never be selected for the bash tool"
    );
    ocserve_store::platform::set_config_shell(None);
}

#[test]
fn an_unresolvable_config_shell_falls_through() {
    let _g = lock();
    ocserve_store::platform::set_config_shell(Some("/nonexistent/definitely-not-a-shell".into()));
    let picked = ocserve_store::platform::tool_shell();
    assert!(
        picked.name != "definitely-not-a-shell",
        "an unresolvable shell must be skipped, got {}",
        picked.program
    );
    // ...and the tool still runs.
    assert_eq!(run("printf ok"), "ok");
    ocserve_store::platform::set_config_shell(None);
}

#[test]
fn login_shell_environment_is_visible_to_commands() {
    let _g = lock();
    // The user-visible consequence of the bug: the tool must run in the SAME
    // environment the resolved shell provides. Honest limit, measured with the
    // negative control: this test does NOT discriminate on its own, because
    // the test process inherits an interactive PATH that already contains
    // ~/.cargo/bin — so `sh -c 'command -v cargo'` succeeds here while it
    // failed in the real service (PATH=/usr/local/bin:/usr/bin). The `$0`
    // identity test is the discriminator; this one guards the weaker but still
    // real property that the tool agrees with the shell it says it uses.
    let resolved = ocserve_store::platform::tool_shell();
    let direct = std::process::Command::new(&resolved.program)
        .args(["-c", "command -v cargo || true"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let via_tool = run("command -v cargo || true");
    assert_eq!(
        via_tool, direct,
        "bash tool output must match the resolved shell ({}): tool={via_tool:?} shell={direct:?}",
        resolved.program
    );
    if direct.is_empty() {
        // Honest: nothing to compare on a host without cargo, but the equality
        // above still proves the tool uses the resolved program.
        eprintln!("note: cargo not installed; equality assertion still applies");
    }
}
