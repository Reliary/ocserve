//! OpenCode v2 config-file compatibility.
//!
//! Upstream v2 (2.2.x) reads `opencode.json` **and** `opencode.jsonc`, walks
//! the project tree (`.opencode/opencode.json(c)` included) and normalizes the
//! deprecated v1 keys into the v2 vocabulary before anything consumes them
//! (`packages/core/src/config/discovery.ts`, `config.ts:loadDirectory`,
//! `normalize.ts`). ocserve 0.7.x read a single `opencode.json` and only the
//! v1 key shapes, so a stock v2 install (whose global file is
//! `~/.config/opencode/opencode.jsonc`) booted with defaults — no providers,
//! no permissions, no MCP servers.
//!
//! This module is the mechanical seam:
//! - [`strip_jsonc`] strips `//`/`/* */` comments and trailing commas, the two
//!   things v2's JSONC parser accepts and `serde_json` does not.
//! - [`global_config_paths`] / [`project_config_paths`] mirror upstream's
//!   discovery order (far directory first, `opencode.json` before
//!   `opencode.jsonc`, `.opencode/` last inside a directory).
//! - [`normalize`] maps the v2 canonical keys onto the internal v1-shaped
//!   vocabulary (`providers`→`provider`, `agents`→`agent`, `commands`→`command`,
//!   `mcp.servers`→`mcp`, permission rules→the internal rule array). Internal
//!   consumers stay unchanged; the v2 keys remain in the document so v2
//!   clients still see them.
//!
//! Bounds: a config document larger than [`MAX_CONFIG_BYTES`] is rejected
//! (MEMORY §6 parse cap), never parsed.

use anyhow::Result;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Maximum accepted config-document size (MEMORY §6 parse cap), re-exported
/// from the shared JSONC parser.
#[cfg(test)]
pub const MAX_CONFIG_BYTES: usize = ocserve_core::jsonc::MAX_CONFIG_BYTES;

/// File names upstream reads inside one config directory, in merge order:
/// `opencode.json` first, `opencode.jsonc` last so it wins on scalars.
pub const GLOBAL_NAMES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

/// Strip JSONC comments and trailing commas. Thin re-export of the one parser
/// in [`ocserve_core::jsonc`] so the HTTP config routes and this loader can
/// never diverge.
#[cfg(test)]
pub fn strip_jsonc(text: &str) -> Result<String> {
    ocserve_core::jsonc::strip(text)
}

/// Parse one config document. Both extensions use the JSONC parser: upstream
/// parses `opencode.json` with the same tolerant reader, and a hand-edited
/// `.json` with a comment should not send the server to defaults.
pub fn parse_document(text: &str) -> Result<Value> {
    ocserve_core::jsonc::parse_object(text)
}

/// `OPENCODE_DISABLE_PROJECT_CONFIG` (upstream env contract) turns the
/// project-tree walk off while the global files still load.
pub fn project_config_disabled() -> bool {
    matches!(
        std::env::var("OPENCODE_DISABLE_PROJECT_CONFIG")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// The two global config files, in merge order (json then jsonc).
pub fn global_config_paths(dir: &Path) -> [PathBuf; 2] {
    [dir.join(GLOBAL_NAMES[0]), dir.join(GLOBAL_NAMES[1])]
}

/// Project config files from the filesystem root down to `cwd`, so a nearer
/// directory wins when layers are merged in order. Upstream also reads
/// `.opencode/` inside each directory; that directory ranks above the plain
/// files (`discovery.ts`: candidates are walked reversed and `.opencode` lands
/// between `.agents` and the file names).
pub fn project_config_paths(cwd: &Path) -> Vec<PathBuf> {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut dirs: Vec<PathBuf> = start.ancestors().map(Path::to_path_buf).collect();
    dirs.reverse(); // root → cwd
    let mut out = Vec::new();
    for dir in dirs {
        for name in GLOBAL_NAMES {
            out.push(dir.join(name));
        }
        for name in GLOBAL_NAMES {
            out.push(dir.join(".opencode").join(name));
        }
    }
    out
}

/// Merge one file layer over another. `plugin`/`plugins` accumulate, because
/// upstream rebuilds the effective plugin list as the deduped union of every
/// layer (a JSONC file must not erase the plugins declared in JSON).
pub fn merge_documents(base: &mut Value, patch: Value) {
    let mut previous: [(&str, Vec<Value>); 2] = [("plugin", Vec::new()), ("plugins", Vec::new())];
    for (key, slot) in previous.iter_mut() {
        if patch.get(*key).is_some_and(Value::is_array) {
            *slot = base
                .get(*key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
    }
    ocserve_http::deep_merge(base, patch);
    for (key, prev) in previous {
        if prev.is_empty() {
            continue;
        }
        let next = base
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(obj) = base.as_object_mut() {
            obj.insert(
                key.to_string(),
                Value::Array(prev.into_iter().chain(next).collect()),
            );
        }
    }
}

/// v1→v2 action names upstream applies in `ConfigMigrateV1.normalizeAction`,
/// reversed: ocserve's permission engine speaks the v1 tool names.
pub fn internal_action(action: &str) -> &str {
    match action {
        "shell" => "bash",
        "subagent" => "task",
        "write" | "patch" | "apply_patch" => "edit",
        other => other,
    }
}

fn permission_rules(value: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    match value {
        // v2 canonical: [{action, resource, effect}]
        Value::Array(items) => {
            for item in items {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let Some(action) = obj.get("action").and_then(Value::as_str) else {
                    continue;
                };
                let Some(effect) = obj.get("effect").and_then(Value::as_str) else {
                    continue;
                };
                out.push(json!({
                    "permission": internal_action(action),
                    "pattern": obj.get("resource").and_then(Value::as_str).unwrap_or("*"),
                    "action": effect,
                }));
            }
        }
        // v1 deprecated: {action: effect | {resource: effect}}
        Value::Object(map) => {
            for (action, target) in map {
                match target {
                    Value::String(effect) => out.push(json!({
                        "permission": internal_action(action),
                        "pattern": "*",
                        "action": effect,
                    })),
                    Value::Object(patterns) => {
                        for (resource, effect) in patterns {
                            let Some(effect) = effect.as_str() else {
                                continue;
                            };
                            out.push(json!({
                                "permission": internal_action(action),
                                "pattern": resource,
                                "action": effect,
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

fn model_string(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    let obj = value.as_object()?;
    let pid = obj.get("providerID").and_then(Value::as_str)?;
    let mid = obj
        .get("model")
        .or_else(|| obj.get("modelID"))
        .and_then(Value::as_str)?;
    let variant = obj.get("variant").and_then(Value::as_str);
    Some(match variant {
        Some(v) if !v.is_empty() => format!("{pid}/{mid}#{v}"),
        _ => format!("{pid}/{mid}"),
    })
}

/// Map one v2 provider entry onto the v1-shaped entry ocserve's derivation
/// reads (`npm` + `options` + `models`). Keeps every unknown field.
fn provider_v1_shape(entry: &Value) -> Value {
    let mut out = entry.as_object().cloned().unwrap_or_default();
    if let Some(package) = entry.get("package").and_then(Value::as_str) {
        out.entry("npm".to_string())
            .or_insert_with(|| json!(package));
    }
    let mut options = out
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(settings) = entry.get("settings").and_then(Value::as_object) {
        for (k, v) in settings {
            options.insert(k.clone(), v.clone());
        }
    }
    if let Some(headers) = entry.get("headers") {
        options.insert("headers".into(), headers.clone());
    }
    if let Some(body) = entry.get("body") {
        options.insert("body".into(), body.clone());
    }
    if !options.is_empty() {
        out.insert("options".into(), Value::Object(options));
    }
    // v2 models may carry `modelID` (the wire id differs from the config key).
    if let Some(models) = entry.get("models").and_then(Value::as_object) {
        let mut mapped = Map::new();
        for (key, model) in models {
            let id = model
                .get("modelID")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_string();
            mapped.insert(id, model.clone());
        }
        out.insert("models".into(), Value::Object(mapped));
    }
    Value::Object(out)
}

/// Map one v2 agent entry onto the internal agent shape: `permissions` (array)
/// becomes the `permission` rule array `merge_agent` already understands,
/// `system` is the prompt, and an object `model` becomes `provider/model`.
fn agent_v1_shape(entry: &Value) -> Value {
    let mut out = entry.as_object().cloned().unwrap_or_default();
    if let Some(model) = entry.get("model").and_then(model_string) {
        out.insert("model".into(), json!(model));
    }
    if let Some(system) = entry.get("system") {
        out.entry("prompt".to_string()).or_insert(system.clone());
    }
    if let Some(perms) = entry.get("permissions") {
        let rules = permission_rules(perms);
        let mut merged = out
            .get("permission")
            .cloned()
            .map(|v| permission_rules(&v))
            .unwrap_or_default();
        merged.extend(rules);
        out.insert("permission".into(), Value::Array(merged));
    }
    Value::Object(out)
}

fn command_v1_shape(entry: &Value) -> Value {
    let mut out = entry.as_object().cloned().unwrap_or_default();
    if let Some(model) = entry.get("model").and_then(model_string) {
        out.insert("model".into(), json!(model));
    }
    if let Some(subagent) = entry.get("subagent").or_else(|| entry.get("subtask")) {
        out.insert("subtask".into(), subagent.clone());
    }
    Value::Object(out)
}

/// Normalize a merged config document in place.
///
/// Both vocabularies remain in the document: the v2 keys are preserved for v2
/// clients, and the v1-shaped mirrors are added (only where the v2 key exists)
/// for ocserve's single derivation path.
pub fn normalize(cfg: &mut Value) {
    let Some(obj) = cfg.as_object_mut() else {
        return;
    };

    if let Some(model) = obj.get("model").and_then(model_string) {
        obj.insert("model".into(), json!(model));
    }

    // providers → provider (native overrides the deprecated map per id)
    if let Some(providers) = obj.get("providers").and_then(Value::as_object).cloned() {
        let mut target = obj
            .get("provider")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (id, entry) in &providers {
            target.insert(id.clone(), provider_v1_shape(entry));
        }
        obj.insert("provider".into(), Value::Object(target));
    }

    // agents → agent
    if let Some(agents) = obj.get("agents").and_then(Value::as_object).cloned() {
        let mut target = obj
            .get("agent")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (name, entry) in &agents {
            target.insert(name.clone(), agent_v1_shape(entry));
        }
        obj.insert("agent".into(), Value::Object(target));
    }

    // commands → command
    if let Some(commands) = obj.get("commands").and_then(Value::as_object).cloned() {
        let mut target = obj
            .get("command")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (name, entry) in &commands {
            target.insert(name.clone(), command_v1_shape(entry));
        }
        obj.insert("command".into(), Value::Object(target));
    }

    // `tools` (deprecated boolean map), the legacy `permission` map and the v2
    // `permissions` array become ONE ordered ruleset in upstream's order
    // (`normalize.ts`: `[...tools, ...legacy, ...native]`, last match wins).
    // Collapsing the order here means the rule engine cannot accidentally give
    // a deprecated `permission` entry precedence over a v2 one.
    let mut rules: Vec<Value> = Vec::new();
    if let Some(tools) = obj.get("tools").and_then(Value::as_object) {
        for (name, enabled) in tools {
            let Some(enabled) = enabled.as_bool() else {
                continue;
            };
            rules.push(json!({
                "permission": internal_action(name),
                "pattern": "*",
                "action": if enabled { "allow" } else { "deny" },
            }));
        }
    }
    if let Some(legacy) = obj.get("permission") {
        rules.extend(permission_rules(legacy));
    }
    if let Some(perms) = obj.get("permissions") {
        rules.extend(permission_rules(perms));
    }
    if !rules.is_empty() {
        obj.insert("permissions".into(), Value::Array(rules));
    }

    // MCP: v2 nests servers under `mcp.servers` and spells the off switch
    // `disabled`. Flatten into the map ocserve-mcp parses. The nested object is
    // removed from the internal view: `parse_config` walks every key of `mcp`,
    // so leaving `servers` in would register a bogus server with that name.
    // The raw file is untouched and `/api/config` still lists it.
    if let Some(servers) = obj
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(Value::as_object)
        .cloned()
    {
        let mut target = obj
            .get("mcp")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        target.remove("servers");
        target.remove("timeout");
        for (name, entry) in &servers {
            let mut server = entry.as_object().cloned().unwrap_or_default();
            if let Some(disabled) = server.remove("disabled") {
                let disabled = disabled.as_bool().unwrap_or(false);
                server.insert("enabled".into(), json!(!disabled));
            }
            target.insert(name.clone(), Value::Object(server));
        }
        obj.insert("mcp".into(), Value::Object(target));
    }

    // v2 moved the nesting depth under `experimental`.
    if !obj.contains_key("subagent_depth")
        && let Some(depth) = obj
            .get("experimental")
            .and_then(|e| e.get("subagent_depth"))
            .cloned()
    {
        obj.insert("subagent_depth".into(), depth);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_keeps_strings_and_removes_comments() {
        let raw = r#"{
            // line comment
            "url": "http://example.com/a//b",
            /* block
               comment */
            "text": "a /* not a comment */ b",
            "list": [1, 2,],
        }"#;
        let cleaned = strip_jsonc(raw).unwrap();
        let v: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(v["url"], "http://example.com/a//b");
        assert_eq!(v["text"], "a /* not a comment */ b");
        assert_eq!(v["list"], json!([1, 2]));
    }

    #[test]
    fn strip_jsonc_rejects_the_unterminated_cases_and_oversize() {
        assert!(strip_jsonc("{ /* open").is_err());
        assert!(strip_jsonc(r#"{"a": "open}"#).is_err());
        let big = "x".repeat(MAX_CONFIG_BYTES + 1);
        assert!(strip_jsonc(&big).is_err());
    }

    #[test]
    fn jsonc_escaped_quote_does_not_end_the_string() {
        let v = parse_document(r#"{"a":"say \"//\" now"}"#).unwrap();
        assert_eq!(v["a"], "say \"//\" now");
    }

    #[test]
    fn global_names_are_json_then_jsonc() {
        let paths = global_config_paths(Path::new("/cfg"));
        assert_eq!(paths[0], Path::new("/cfg/opencode.json"));
        assert_eq!(paths[1], Path::new("/cfg/opencode.jsonc"));
    }

    #[test]
    fn project_paths_run_root_to_cwd_and_include_dot_opencode() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let paths = project_config_paths(&nested);
        let last4: Vec<_> = paths[paths.len() - 4..]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(
            last4,
            vec![
                nested.join("opencode.json").display().to_string(),
                nested.join("opencode.jsonc").display().to_string(),
                nested.join(".opencode/opencode.json").display().to_string(),
                nested
                    .join(".opencode/opencode.jsonc")
                    .display()
                    .to_string(),
            ]
        );
        assert!(paths.iter().any(|p| p.ends_with("a/opencode.json")));
    }

    #[test]
    fn normalize_maps_v2_permissions_tools_and_actions_to_internal() {
        let mut cfg = json!({
            "permissions": [
                {"action": "shell", "resource": "rm -rf *", "effect": "deny"},
                {"action": "task", "resource": "explore", "effect": "allow"},
                {"action": "write", "resource": "*.rs", "effect": "ask"}
            ],
            "tools": {"bash": false, "read": true}
        });
        normalize(&mut cfg);
        let rules = cfg["permissions"].as_array().unwrap();
        // tools first, then the canonical v2 list
        assert_eq!(rules[0]["permission"], "bash");
        assert_eq!(rules[0]["action"], "deny");
        assert_eq!(rules[1]["permission"], "read");
        assert_eq!(rules[2]["permission"], "bash");
        assert_eq!(rules[3]["permission"], "task");
        assert_eq!(rules[4]["permission"], "edit");
        assert_eq!(rules[4]["pattern"], "*.rs");
    }

    #[test]
    fn normalize_maps_v2_providers_agents_commands_and_model() {
        let mut cfg = json!({
            "model": {"providerID": "p", "model": "m", "variant": "fast"},
            "providers": {
                "p": {
                    "name": "P",
                    "package": "@ai-sdk/openai-compatible",
                    "settings": {"baseURL": "http://x/v1", "apiKey": "k"},
                    "models": {"alias": {"modelID": "real", "name": "Real"}}
                }
            },
            "agents": {
                "reviewer": {
                    "description": "Reviews",
                    "system": "be strict",
                    "model": {"providerID": "p", "model": "m"},
                    "permissions": [{"action": "read", "resource": "*", "effect": "allow"}]
                }
            },
            "commands": {"ship": {"template": "ship $ARGUMENTS", "subagent": true}}
        });
        normalize(&mut cfg);
        assert_eq!(cfg["model"], "p/m#fast");
        assert_eq!(cfg["provider"]["p"]["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(cfg["provider"]["p"]["options"]["baseURL"], "http://x/v1");
        assert_eq!(cfg["provider"]["p"]["options"]["apiKey"], "k");
        assert!(cfg["provider"]["p"]["models"].get("real").is_some());
        assert_eq!(cfg["agent"]["reviewer"]["prompt"], "be strict");
        assert_eq!(cfg["agent"]["reviewer"]["model"], "p/m");
        assert_eq!(
            cfg["agent"]["reviewer"]["permission"][0]["permission"],
            "read"
        );
        assert_eq!(cfg["command"]["ship"]["subtask"], true);
    }

    #[test]
    fn normalize_flattens_mcp_servers_and_the_disabled_switch() {
        let mut cfg = json!({
            "mcp": {
                "servers": {
                    "off": {"type": "local", "command": ["x"], "disabled": true},
                    "on": {"type": "remote", "url": "https://x"}
                }
            }
        });
        normalize(&mut cfg);
        assert_eq!(cfg["mcp"]["off"]["enabled"], false);
        assert_eq!(
            cfg["mcp"]["on"]["enabled"],
            json!(null),
            "absent disabled stays absent"
        );
        // the nested object is removed from the internal view: leaving it in
        // would make parse_config register a server literally named "servers".
        assert!(cfg["mcp"].get("servers").is_none());
        assert!(cfg["mcp"]["off"].is_object());
    }

    #[test]
    fn normalize_orders_tools_legacy_then_native_rules() {
        let mut cfg = json!({
            "tools": {"shell": false},
            "permission": {"bash": "ask"},
            "permissions": [{"action": "shell", "resource": "*", "effect": "allow"}]
        });
        normalize(&mut cfg);
        let rules = cfg["permissions"].as_array().unwrap();
        let effects: Vec<_> = rules
            .iter()
            .map(|r| {
                (
                    r["permission"].as_str().unwrap(),
                    r["action"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            effects,
            vec![("bash", "deny"), ("bash", "ask"), ("bash", "allow")],
            "upstream order is tools, legacy permission, native permissions"
        );
    }

    #[test]
    fn normalize_reads_experimental_subagent_depth() {
        let mut cfg = json!({"experimental": {"subagent_depth": 3}});
        normalize(&mut cfg);
        assert_eq!(cfg["subagent_depth"], 3);
    }
}
