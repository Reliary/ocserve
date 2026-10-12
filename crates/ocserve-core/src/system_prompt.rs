//! Model-facing system-prompt composition (v1 parity, K-CAPDIFF).
//!
//! Upstream builds the provider system message in two places:
//!   - `session/llm/request.ts:57-66` — `agent.prompt ? [agent.prompt] :
//!     SystemPrompt.provider(model)`, then `...input.system`, then
//!     `user.system`, all `.filter(x => x).join("\n")`.
//!   - `session/prompt.ts:1258-1267` — `input.system = [...env, ...instructions,
//!     ...mcpInstructions?, ...skills?]`.
//!   - `session/system.ts:provider()` selects the base prompt file by model id.
//!
//! Before this module ocserve shipped a ~250-byte `BUILD_SYSTEM_BLURB` where
//! freeze sends the full ~9.5 KB `default.txt` + env + skills block. capdiff
//! (bench/capdiff) captured both arms' real provider requests and surfaced it
//! on its first run — the provider boundary had no oracle.
//!
//! The base prompt files are VENDORED from the freeze tag by
//! `scripts/extract-prompts.sh` (never hand-copied), so a version bump is one
//! command + the gates.

/// Select the upstream base prompt file for a model, mirroring
/// `system.ts:provider()`. Returns the vendored text.
pub fn provider_prompt(model_id: &str, provider_id: &str) -> &'static str {
    let m = model_id;
    let ml = model_id.to_lowercase();
    let pl = provider_id.to_lowercase();
    if m.contains("muse") {
        // meta.txt uses {{MODEL_NAME}}; caller substitutes (see base_prompt).
        return include_str!("../assets/prompt/meta.txt");
    }
    if m.contains("gpt-4") || m.contains("o1") || m.contains("o3") {
        return include_str!("../assets/prompt/beast.txt");
    }
    if m.contains("gpt") {
        if m.contains("gpt-6") {
            return include_str!("../assets/prompt/gpt-astra.txt");
        }
        if m.contains("codex") {
            return include_str!("../assets/prompt/codex.txt");
        }
        return include_str!("../assets/prompt/gpt.txt");
    }
    if m.contains("gemini-") {
        return include_str!("../assets/prompt/gemini.txt");
    }
    if m.contains("claude") {
        return include_str!("../assets/prompt/anthropic.txt");
    }
    if ml.contains("trinity") {
        return include_str!("../assets/prompt/trinity.txt");
    }
    if ml.contains("kimi") || ["kimi-for-coding", "moonshotai", "moonshotai-cn"].contains(&pl.as_str())
    {
        return include_str!("../assets/prompt/kimi.txt");
    }
    include_str!("../assets/prompt/default.txt")
}

/// Base prompt for an agent: an explicit agent prompt wins, else the
/// model-selected provider prompt (upstream `request.ts:60`).
pub fn base_prompt(agent_prompt: Option<&str>, model_id: &str, provider_id: &str) -> String {
    if let Some(p) = agent_prompt
        && !p.is_empty()
    {
        return p.to_string();
    }
    let raw = provider_prompt(model_id, provider_id);
    // meta.txt is the only templated file (system.ts:25 replaces {{MODEL_NAME}})
    if raw.contains("{{MODEL_NAME}}") {
        let name = if model_id.contains("muse-glimmer") {
            "Muse Glimmer"
        } else {
            "Muse Spark"
        };
        return raw.replace("{{MODEL_NAME}}", name);
    }
    raw.to_string()
}

/// Environment block (system.ts:environment, v1.18.31 lines 69-103). The
/// reference/skills blocks are added by the caller.
#[allow(clippy::too_many_arguments)]
pub fn environment_block(
    model_id: &str,
    provider_id: &str,
    directory: &str,
    worktree: &str,
    is_git: bool,
    platform: &str,
    today: &str,
) -> String {
    format!(
        "You are powered by the model named {model_id}. The exact model ID is {provider_id}/{model_id}\n\
         Here is some useful information about the environment you are running in:\n\
         <env>\n\
         \x20 Working directory: {directory}\n\
         \x20 Workspace root folder: {worktree}\n\
         \x20 Is directory a git repo: {}\n\
         \x20 Platform: {platform}\n\
         \x20 Today's date: {today}\n\
         </env>",
        if is_git { "yes" } else { "no" },
    )
}

/// Skills block (system.ts:skills, lines 107-119): header + `Skill.fmt(list,
/// {verbose:true})` — the XML `<available_skills>` rendering, sorted by name,
/// location HTML-escaped, or "No skills are currently available." when no
/// skill has a description.
pub fn skills_block(skills: &[(String, String, String)]) -> Option<String> {
    let mut described: Vec<&(String, String, String)> =
        skills.iter().filter(|(_, d, _)| !d.is_empty()).collect();
    let header = "Skills provide specialized instructions and workflows for specific tasks.\n\
                  Use the skill tool to load a skill when a task matches its description.";
    if described.is_empty() {
        return Some(format!("{header}\nNo skills are currently available."));
    }
    described.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from(header);
    out.push_str("\n<available_skills>");
    for (name, description, location) in described {
        out.push_str("\n  <skill>");
        out.push_str(&format!("\n    <name>{name}</name>"));
        out.push_str(&format!("\n    <description>{description}</description>"));
        out.push_str(&format!("\n    <location>{}</location>", escape_html(location)));
        out.push_str("\n  </skill>");
    }
    out.push_str("\n</available_skills>");
    Some(out)
}

/// Upstream `escapeHtml` (skill/index.ts fmt): & < > (and the quote entities).
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Assemble the final system message. `input_system` entries (env, mcp,
/// skills, …) are joined after the base with "\n", exactly as upstream's
/// `.filter(x => x).join("\n")` (request.ts:63-66).
pub fn compose(base: &str, input_system: &[Option<String>], user_system: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(base.to_string());
    for s in input_system.iter().flatten() {
        if !s.is_empty() {
            parts.push(s.clone());
        }
    }
    if let Some(u) = user_system
        && !u.is_empty()
    {
        parts.push(u.to_string());
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_selects_provider_prompt() {
        assert!(provider_prompt("claude-sonnet-4", "anthropic").contains("OpenCode"));
        assert_eq!(provider_prompt("claude-sonnet-4", "anthropic"),
                   include_str!("../assets/prompt/anthropic.txt"));
        assert_eq!(provider_prompt("stub-1", "stub"),
                   include_str!("../assets/prompt/default.txt"));
        assert_eq!(provider_prompt("gpt-4o", "openai"),
                   include_str!("../assets/prompt/beast.txt"));
        assert_eq!(provider_prompt("gpt-5", "openai"),
                   include_str!("../assets/prompt/gpt.txt"));
        assert_eq!(provider_prompt("gemini-3-pro", "google"),
                   include_str!("../assets/prompt/gemini.txt"));
    }

    #[test]
    fn explicit_agent_prompt_wins() {
        let b = base_prompt(Some("You are the explore agent."), "stub-1", "stub");
        assert_eq!(b, "You are the explore agent.");
    }

    #[test]
    fn empty_agent_prompt_falls_through() {
        let b = base_prompt(Some(""), "stub-1", "stub");
        assert_eq!(b, include_str!("../assets/prompt/default.txt"));
    }

    #[test]
    fn environment_block_matches_capture_shape() {
        let e = environment_block("stub-1", "stub", "/", "/", false, "linux",
                                  "Mon Oct 12 2026");
        assert!(e.starts_with(
            "You are powered by the model named stub-1. The exact model ID is stub/stub-1\n"));
        assert!(e.contains("\n<env>\n  Working directory: /\n"));
        assert!(e.contains("  Is directory a git repo: no\n"));
        assert!(e.contains("  Today's date: Mon Oct 12 2026\n</env>"));
        assert!(!e.ends_with('\n'));
    }

    #[test]
    fn env_then_skills_joined_by_newline() {
        let base = "BASE";
        let env = environment_block("m", "p", "/w", "/w", true, "linux", "d");
        let skills = skills_block(&[("s".into(), "d".into(), "l".into())]);
        let sys = compose(base, &[Some(env.clone()), skills.clone()], None);
        assert!(sys.starts_with("BASE\nYou are powered by the model named m"));
        assert!(sys.contains("</env>\nSkills provide specialized"));
        assert!(sys.contains("<name>s</name>"));
    }

    #[test]
    fn compose_skips_empty_parts() {
        let sys = compose("B", &[None, Some(String::new())], Some("U"));
        assert_eq!(sys, "B\nU");
    }
}
