//! Runtime assembly: loads the user's opencode config/auth/models-cache and
//! derives every config-derived route payload (PLAN F1 wire surface).
//!
//! Contract facts derived from live upstream 1.18.31 + v1 source (2026-10-01):
//! - /config: raw opencode.json + plugin-registered agents (plugin part = M4;
//!   manifest uses keys_subset with raw-file keys until then)
//! - /config/providers: state.providers = auth.json order, then built-in
//!   `opencode`, then config-only entries; source = config > api > custom;
//!   models = cache models + config model overrides; key from auth.json
//! - /provider: cache order overlaid by state providers in place, config-only
//!   appended; default = priority-sort per provider; connected = state order
//!   filtered through the all[] list
//! - /agent: default_agent first, then name asc; native + config agents
//! - /command: built-in init/review first, then config commands
//!   (skills discovery separate, not in TUI hit-set)
//! - /experimental/console, /experimental/capabilities: static captures

use anyhow::{Context, Result, anyhow};
use ocserve_http::LlmRegistry;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Priority list from v1 provider.ts sort() — determines default model choice.
const MODEL_PRIORITY: &[&str] = &["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];

pub struct Runtime {
    /// Data dir (auth-overlay lives here) — kept so llm_registry layers the
    /// same auth as load_for (W5 read-precedence rule).
    pub data_dir: std::path::PathBuf,
    pub config: Value,
    /// Parsed models catalog — read ONCE in load_for and reused by
    /// llm_registry (A1: the old re-read double-parsed the 5.3 MB file per
    /// reload and was the proven OOM-spike class, 2026-10-05 14:12 kill).
    pub catalog: Value,
    /// Default-agent-first order (GET /agent, v1 wire).
    pub agent: Vec<Value>,
    /// Declaration order: natives then config agents (GET /api/agent, v2 wire).
    pub api_agent: Vec<Value>,
    pub command: Vec<Value>,
    pub config_providers: Value,
    pub provider: Value,
    pub console: Value,
    pub capabilities: Value,
}

fn read_json(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// JSON string escaping matching `JSON.stringify(s).slice(1, -1)` — the way
/// upstream splices a `{file:…}` payload into the raw config text before parse
/// (config/variable.ts:52).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Upstream `ConfigVariable.substitute` (config/variable.ts:33-72): applied to
/// the RAW config text before parsing. `{env:NAME}` expands to the raw value
/// (upstream parity — not escaped); `{file:path}` reads a file relative to the
/// config's directory (`~/` expanded), trims it and JSON-escapes it. A missing
/// referenced file is an error (upstream's default `missing:"error"`); the
/// caller degrades to defaults loudly rather than server-defaults silently.
///
/// Issue #28: without this, a documented `"apiKey": "{env:VAR}"` reached the
/// provider as an empty Bearer token, because the token was stored verbatim
/// and no layer ever expanded it.
fn expand_config_vars(
    text: &str,
    base_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while let Some(rest) = text.get(i..) {
        let env_at = rest.find("{env:");
        let file_at = rest.find("{file:");
        let (at, is_env) = match (env_at, file_at) {
            (Some(e), Some(f)) if f < e => (f, false),
            (Some(e), _) => (e, true),
            (None, Some(f)) => (f, false),
            (None, None) => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..at]);
        let start = i + at;
        let prefix = if is_env { 5 } else { 6 };
        let after = &text[start + prefix..];
        let Some(close) = after.find('}') else {
            out.push_str(&text[start..]);
            break;
        };
        let name = &after[..close];
        if is_env {
            out.push_str(&env(name).unwrap_or_default());
        } else {
            let mut raw = name.to_string();
            if let Some(tail) = raw.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                raw = format!("{home}/{tail}");
            }
            let path = if Path::new(&raw).is_absolute() {
                std::path::PathBuf::from(&raw)
            } else {
                base_dir.join(&raw)
            };
            let content = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "bad file reference \"{{file:{name}}}\": {} ({e})",
                    path.display()
                )
            })?;
            out.push_str(&json_escape(content.trim()));
        }
        i = start + prefix + close + 1;
    }
    Ok(out)
}

/// Read + expand + parse one config file. `{file:…}` failure is loud and
/// degrades to defaults (same contract as a corrupt config — the drop-in must
/// still boot; the bad reference is named in the log). Upstream aborts the
/// load with InvalidError; named divergence `D-CONFIG-VAR` (PLAN §18).
fn read_config_resolved(path: &Path) -> Value {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no global config at {} — using defaults", path.display());
            return json!({});
        }
        Err(e) => {
            tracing::warn!(
                "global config unreadable ({}), using defaults: {e}",
                path.display()
            );
            return json!({});
        }
    };
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let text = match expand_config_vars(&raw, base, &|n| std::env::var(n).ok()) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("global config {}: {e}; using defaults", path.display());
            return json!({});
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "global config unreadable ({}), using defaults: {e}",
                path.display()
            );
            json!({})
        }
    }
}

/// Global config read with upstream parity (config.ts: missing file → empty,
/// unreadable/corrupt → "using defaults" + continue). Found by CI 2026-10-08:
/// `serve` on a bare machine (no `~/.config/opencode/opencode.json`) died at
/// boot — a fresh install of the drop-in server must start with defaults, and
/// upstream treats both cases as defaults, never as fatal. Per-user errors
/// stay loud: the missing/corrupt file is logged with its path.
fn read_global_config(path: &Path) -> Value {
    read_config_resolved(path)
}

/// Layer the two explicit config channels upstream reads on top of the
/// file-derived config (config.ts: `OPENCODE_CONFIG` then
/// `OPENCODE_CONFIG_CONTENT`, the latter a final local-scope merge).
///
/// This is how launchers inject plugins: Paseo rewrites
/// `OPENCODE_CONFIG_CONTENT` with its bridge plugin tuple. Ignoring the
/// variable silently dropped the whole plugin surface (observed 2026-11-09).
/// The channels are explicit operator intent, so a malformed value is a loud
/// boot error (upstream dies on it too) rather than a silent ignore; a MISSING
/// `OPENCODE_CONFIG` file warns and contributes nothing (upstream's
/// readConfigFile returns undefined there).
///
/// Never logs the value — it carries launcher tokens.
fn layer_env_config(
    mut cfg: Value,
    config_path: Option<std::path::PathBuf>,
    content: Option<&str>,
    base_dir: &Path,
) -> Result<Value> {
    if let Some(path) = config_path {
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let base = path.parent().unwrap_or(base_dir);
                let text = expand_config_vars(&raw, base, &|n| std::env::var(n).ok())
                    .map_err(|e| anyhow!("OPENCODE_CONFIG {}: {e}", path.display()))?;
                let v: Value = serde_json::from_str(&text)
                    .with_context(|| format!("parse OPENCODE_CONFIG {}", path.display()))?;
                if !v.is_object() {
                    anyhow::bail!("OPENCODE_CONFIG {}: expected a JSON object", path.display());
                }
                tracing::info!(
                    "loaded explicit config from OPENCODE_CONFIG ({})",
                    path.display()
                );
                merge_config_layer(&mut cfg, v);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    "OPENCODE_CONFIG points at a missing file ({}) — ignored",
                    path.display()
                );
            }
            Err(e) => {
                return Err(anyhow!("read OPENCODE_CONFIG {}: {e}", path.display()));
            }
        }
    }
    if let Some(raw) = content.map(str::trim).filter(|s| !s.is_empty()) {
        let text = expand_config_vars(raw, base_dir, &|n| std::env::var(n).ok())
            .map_err(|e| anyhow!("OPENCODE_CONFIG_CONTENT: {e}"))?;
        let v: Value = serde_json::from_str(&text)
            .context("parse OPENCODE_CONFIG_CONTENT (invalid JSON; value is not logged)")?;
        if !v.is_object() {
            anyhow::bail!("OPENCODE_CONFIG_CONTENT: expected a JSON object");
        }
        tracing::info!(
            "loaded explicit config from OPENCODE_CONFIG_CONTENT ({} bytes)",
            raw.len()
        );
        merge_config_layer(&mut cfg, v);
    }
    Ok(cfg)
}

/// Merge one config layer over `base`.
///
/// `plugin` / `plugins` ACCUMULATE instead of replacing. Upstream rebuilds the
/// effective list as the deduped union of every layer's declarations
/// (`mergePluginOrigins` / v2 `external.ts`), so a launcher that injects its
/// bridge must not erase the plugins the user declared in `opencode.json`
/// (arrays still replace for every other key, matching `mergeDeep`).
fn merge_config_layer(base: &mut Value, patch: Value) {
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

/// models catalog read with upstream self-heal semantics (models-dev.ts
/// loadFromDisk): unreadable/corrupt → remove best-effort + treat missing
/// so the hourly refresh refetches. NEVER fails the caller — a broken cache
/// file must not fail boot (the old `read_json(cache)?` did exactly that,
/// where upstream deletes and refetches).
fn read_catalog(path: &Path) -> Value {
    if !path.exists() {
        return json!({});
    }
    match read_json(path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "models catalog unreadable ({e:#}) — removing for refetch (models-dev parity)"
            );
            if let Err(de) = std::fs::remove_file(path) {
                tracing::warn!("models catalog removal failed: {de:#}");
            }
            json!({})
        }
    }
}

/// Layered auth: legacy auth.json with ocserve's auth-overlay merged over it
/// (W5 read-precedence: overlay wins; ocserve NEVER writes the legacy file).
/// load_for AND llm_registry read auth through this — one layering rule, so
/// keys added via PUT /auth reach actual prompts, not just the display routes
/// (was a real gap: registry read legacy-only).
fn load_auth(data_dir: &Path) -> Result<Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    let auth_path = Path::new(&home).join(".local/share/opencode/auth.json");
    let auth = if auth_path.exists() {
        read_json(&auth_path)?
    } else {
        json!({})
    };
    let overlay_path = data_dir.join("auth-overlay.json");
    if overlay_path.exists() {
        let overlay = read_json(&overlay_path)?;
        Ok(layer_auth(auth, overlay))
    } else {
        Ok(auth)
    }
}

/// Pure layering rule (unit-tested): overlay patches OVER legacy — overlay
/// wins on scalars, legacy-only entries survive (deep_merge semantics).
fn layer_auth(mut legacy: Value, overlay: Value) -> Value {
    ocserve_http::deep_merge(&mut legacy, overlay);
    legacy
}

/// The exact files Runtime reads (load_for + llm_registry) — the hot-reload
/// watch set (H1). Keep in sync when load_for gains a read.
pub fn watch_paths(data_dir: &Path) -> Vec<std::path::PathBuf> {
    // OPENCODE_CONFIG is a real read input now; a changed file must reload.
    // OPENCODE_CONFIG_CONTENT is process env, not a file — restart to change.
    let explicit = std::env::var_os("OPENCODE_CONFIG")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let home = std::env::var("HOME").unwrap_or_default();
    let config_json = ocserve_store::platform::opencode_config_dir()
        .map(|d| d.join("opencode.json"))
        .unwrap_or_else(|| Path::new(&home).join(".config/opencode/opencode.json"));
    watch_paths_with(data_dir, explicit, config_json)
}

/// Env-free variant so the watch-set test never depends on the ambient shell.
fn watch_paths_with(
    data_dir: &Path,
    opencode_config: Option<std::path::PathBuf>,
    config_json: std::path::PathBuf,
) -> Vec<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let h = Path::new(&home);
    let mut paths = vec![
        config_json,
        h.join(".config/ocserve/config.json"),
        h.join(".local/share/opencode/auth.json"),
        data_dir.join("auth-overlay.json"),
        // K-MODELS: the catalog ocserve READS (source-aware + fixture flag)
        crate::models_dev::read_path(),
        h.join(".local/state/opencode/model.json"),
    ];
    if let Some(p) = opencode_config {
        paths.push(p);
    }
    paths
}

use std::path::Path;

/// Priority-sort replicating v1 provider.ts `sort()` (multi-pass stable):
/// id desc → "latest" asc → priority-index desc.
pub fn default_model(models: &BTreeMap<String, Value>) -> Option<String> {
    let mut ids: Vec<&String> = models.keys().collect();
    ids.sort(); // base order for stable passes
    ids.sort_by(|a, b| b.as_str().cmp(a.as_str())); // id desc
    ids.sort_by_key(|id| if id.contains("latest") { 0 } else { 1 }); // latest asc
    ids.sort_by_key(|id| {
        // earlier priority list item = higher rank → negate for ascending sort
        -(MODEL_PRIORITY
            .iter()
            .position(|p| id.contains(p))
            .unwrap_or(0) as i64)
    });
    ids.first().map(|s| (*s).clone())
}

fn model_map(v: &Value) -> BTreeMap<String, Value> {
    v.as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Transform a models.dev cache model entry into the served wire shape
/// (verified against live /provider output for deepinfra/tencent-Hy3).
fn transform_cache_model(pid: &str, npm: &str, mid: &str, m: &Value) -> Value {
    let modal = |side: &str, kind: &str| -> bool {
        m.pointer(&format!("/modalities/{}/{kind}", side))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some(side_kind(kind))))
            .unwrap_or(false)
    };
    let _ = modal;
    let has = |path: &str, kind: &str| -> bool {
        m.pointer(path)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some(kind)))
            .unwrap_or(false)
    };
    let input = |k: &str| has("/modalities/input", k);
    let output = |k: &str| has("/modalities/output", k);
    // cost: cache_read → cache.read, cache_write → cache.write; tiers get the
    // same shape (verified against live /provider ByteDance/Seed-2.0-code).
    let cost_obj = |m: &Value| -> Value {
        let mut c = json!({
            "input": m.pointer("/cost/input").cloned().unwrap_or(Value::Null),
            "output": m.pointer("/cost/output").cloned().unwrap_or(Value::Null),
            "cache": {
                "read": m.pointer("/cost/cache_read").cloned().unwrap_or(json!(0)),
                "write": m.pointer("/cost/cache_write").cloned().unwrap_or(json!(0)),
            },
        });
        if let Some(tiers) = m.pointer("/cost/tiers").and_then(|t| t.as_array()) {
            let wire_tiers: Vec<Value> = tiers
                .iter()
                .map(|t| {
                    json!({
                        "input": t.get("input").cloned().unwrap_or(Value::Null),
                        "output": t.get("output").cloned().unwrap_or(Value::Null),
                        "cache": {
                            "read": t.pointer("/cache_read").cloned().unwrap_or(json!(0)),
                            "write": t.pointer("/cache_write").cloned().unwrap_or(json!(0)),
                        },
                        "tier": t.get("tier").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            c["tiers"] = Value::Array(wire_tiers);
        }
        c
    };
    json!({
        "id": mid,
        "providerID": pid,
        "api": {"id": mid, "url": "", "npm": npm},
        "name": m.get("name").and_then(|v| v.as_str()).unwrap_or(mid),
        "family": m.get("family").cloned().unwrap_or(Value::Null),
        "capabilities": {
            "temperature": m.get("temperature").and_then(|v| v.as_bool()).unwrap_or(false),
            "reasoning": m.get("reasoning").and_then(|v| v.as_bool()).unwrap_or(false),
            "attachment": m.get("attachment").and_then(|v| v.as_bool()).unwrap_or(false),
            "toolcall": m.get("tool_call").and_then(|v| v.as_bool()).unwrap_or(false),
            "input": {"text": input("text"), "audio": input("audio"), "image": input("image"), "video": input("video"), "pdf": input("pdf")},
            "output": {"text": output("text"), "audio": output("audio"), "image": output("image"), "video": output("video"), "pdf": output("pdf")},
            // v1 provider.ts:1359: object | bool | false (cache stores object/bool)
            "interleaved": m.get("interleaved").cloned().unwrap_or(json!(false)),
        },
        "cost": cost_obj(m),
        "limit": m.get("limit").cloned().unwrap_or(json!({})),
        "status": m.get("status").and_then(|v| v.as_str()).unwrap_or("active"),
        "options": m.get("options").cloned().unwrap_or(json!({})),
        "headers": m.get("headers").cloned().unwrap_or(json!({})),
        "release_date": m.get("release_date").cloned().unwrap_or(Value::Null),
        "variants": effort_variants(m),
    })
}

/// variants from reasoning_options[{type:"effort", values:[...]}] →
/// {value: {reasoningEffort: value}} (verified vs live ByteDance/Seed-2.0-code).
fn effort_variants(m: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(opts) = m.get("reasoning_options").and_then(|v| v.as_array()) {
        for opt in opts {
            if opt.get("type").and_then(|v| v.as_str()) == Some("effort")
                && let Some(values) = opt.get("values").and_then(|v| v.as_array())
            {
                for val in values {
                    if let Some(v) = val.as_str() {
                        out.insert(v.to_string(), json!({"reasoningEffort": v}));
                    }
                }
            }
        }
    }
    Value::Object(out)
}

fn side_kind(kind: &str) -> &str {
    kind
}

/// Transform a config-defined model block (different shape: {limit,name,...}).
fn transform_config_model(pid: &str, mid: &str, m: &Value) -> Value {
    // Freeze always emits `limit.context`+`limit.output` (provider.ts:1609
    // defaults both to 0); the v1 spec marks them required. `input` is
    // OPTIONAL and omitted when absent (freeze live shape: {context,output}).
    // Carrying the raw config block verbatim left `{}` for models with no
    // limit → spec violation (caught by P4 on the live config, 2026-10-09);
    // always-emitting `input: null` then diverged from freeze (pair, same day).
    let src = m.get("limit");
    let mut limit = json!({
        "context": src.and_then(|l| l.get("context")).cloned().unwrap_or(json!(0)),
        "output": src.and_then(|l| l.get("output")).cloned().unwrap_or(json!(0)),
    });
    if let Some(input) = src.and_then(|l| l.get("input")) {
        limit["input"] = input.clone();
    }
    json!({
        "id": mid,
        "providerID": pid,
        "api": {"id": mid, "url": "", "npm": m.get("npm").and_then(|v| v.as_str()).unwrap_or("")},
        "name": m.get("name").and_then(|v| v.as_str()).unwrap_or(mid),
        "family": m.get("family").cloned().unwrap_or(Value::Null),
        "capabilities": {
            "temperature": m.get("temperature").and_then(|v| v.as_bool()).unwrap_or(true),
            "reasoning": m.get("reasoning").and_then(|v| v.as_bool()).unwrap_or(false),
            "attachment": m.get("attachment").and_then(|v| v.as_bool()).unwrap_or(false),
            "toolcall": m.get("tool_call").and_then(|v| v.as_bool()).unwrap_or(false),
            "input": {
                "text": m.pointer("/modalities/input").and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str()==Some("text"))).unwrap_or(true),
                "audio": m.pointer("/modalities/input").and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str()==Some("audio"))).unwrap_or(false),
                "image": m.pointer("/modalities/input").and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str()==Some("image"))).unwrap_or(false),
                "video": m.pointer("/modalities/input").and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str()==Some("video"))).unwrap_or(false),
                "pdf": m.pointer("/modalities/input").and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str()==Some("pdf"))).unwrap_or(false),
            },
            "output": {"text": true, "audio": false, "image": false, "video": false, "pdf": false},
            "interleaved": m.get("interleaved").cloned().unwrap_or(json!(false)),
        },
        "cost": {
            "input": m.pointer("/cost/input").cloned().unwrap_or(Value::Null),
            "output": m.pointer("/cost/output").cloned().unwrap_or(Value::Null),
            "cache": {"read": m.pointer("/cost/cache_read").cloned().unwrap_or(json!(0)), "write": json!(0)},
        },
        "limit": limit,
        "status": "active",
        "options": m.get("options").cloned().unwrap_or(json!({})),
        "headers": m.get("headers").cloned().unwrap_or(json!({})),
        "release_date": m.get("release_date").cloned().unwrap_or(Value::Null),
        "variants": m.get("variants").cloned().unwrap_or(json!({})),
    })
}

fn sort_json_keys(m: &serde_json::Map<String, Value>) -> BTreeMap<String, Value> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Flatten Permission.fromConfig: string → single rule; object → per-pattern rules.
pub fn perm_from_config(cfg: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(obj) = cfg.as_object() else {
        return out;
    };
    for (key, value) in obj {
        match value {
            Value::String(action) => out.push(json!({
                "permission": key, "pattern": "*", "action": action
            })),
            Value::Object(pats) => {
                for (pat, action) in pats {
                    out.push(json!({
                        "permission": key,
                        "pattern": expand_pattern(pat),
                        "action": action
                    }));
                }
            }
            _ => {}
        }
    }
    out
}

fn expand_pattern(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if p == "~" {
        return home;
    }
    if let Some(rest) = p.strip_prefix("$HOME/") {
        return format!("{home}/{rest}");
    }
    if let Some(rest) = p.strip_prefix("$HOME") {
        return format!("{home}{rest}");
    }
    p.to_string()
}

/// Effective default model: state `recent[]` then `favorite[]` entries,
/// but ONLY ones whose provider has an endpoint (registry-built map).
/// Endpoint-less state picks made every model-less prompt return 400
/// "no endpoint configured for provider …" and poisoned the currentModel
/// route the app echoes back (live repro 2026-10-04: state recent[0] =
/// opencode/big-pickle with no `opencode` endpoint in the registry).
/// Endpoint-ful but UNCATHALOGUED picks fail later at the provider (the
/// first fix landed on openrouter/stealth/ox-alpha — known-dead model, W6
/// warned in the same boot), so candidates must ALSO be in the catalog.
/// Deterministic final fallback: lexicographically-first endpoint + its
/// first catalog model (was HashMap iteration order — nondeterministic).
pub(crate) fn pick_default_model(
    state: &Value,
    cfg: &Value,
    config_providers: &Value,
    endpoints: &std::collections::HashMap<String, (String, String)>,
) -> (String, String) {
    let mut candidates: Vec<(String, String)> = Vec::new();
    for pointer in ["/recent", "/favorite"] {
        if let Some(arr) = state.pointer(pointer).and_then(|v| v.as_array()) {
            for m in arr {
                let p = m.get("providerID").and_then(|v| v.as_str()).unwrap_or("");
                let mid = m.get("modelID").and_then(|v| v.as_str()).unwrap_or("");
                if !p.is_empty() && !mid.is_empty() {
                    candidates.push((p.to_string(), mid.to_string()));
                }
            }
        }
    }
    if let Some(hit) = candidates.iter().find(|(p, m)| {
        endpoints.contains_key(p)
            && catalog_contains(config_providers, cfg, p, m)
            && !catalog_model_deprecated(config_providers, p, m)
    }) {
        return hit.clone();
    }
    let Some(pid) = endpoints.keys().min() else {
        return (String::new(), String::new());
    };
    let model = first_catalog_model(config_providers, cfg, pid);
    (pid.clone(), model)
}

/// B2 (K-MODEL-STATE): opencode public-tier endpoint — freeze
/// provider.ts:185-240. Key precedence: env OPENCODE_API_KEY → config
/// options.apiKey → auth.json key → **literal "public" (keyless)**.
/// Base URL = the models-cache `api` field (models.dev:
/// https://opencode.ai/zen/v1).
///
/// The 2026-10-04 "public tier is dead" verdict was WRONG — our probe was
/// malformed (missing composite UA + tools array). Proven 2026-10-05 by
/// `bench/zen-probe` (FINDINGS: P5 = exact ocserve wire completes 200;
/// P7 = same minus UA fails; P6 = no tools fails; P8 = tools+none passes).
/// Keyless now installs with `Bearer public`; `OCSERVE_ZEN_KEYLESS=0`
/// restores the pre-port behavior (no keyless endpoint → deepseek default).
pub(crate) fn opencode_public_endpoint(
    cfg: &Value,
    auth: &Value,
    cache: &Value,
) -> Option<(String, String)> {
    let keyless = !matches!(std::env::var("OCSERVE_ZEN_KEYLESS").as_deref(), Ok("0"));
    opencode_public_endpoint_inner(cfg, auth, cache, keyless)
}

/// Pure core (no env — unit tests pass the kill-switch state directly so
/// parallel tests never race on process env).
pub(crate) fn opencode_public_endpoint_inner(
    cfg: &Value,
    auth: &Value,
    cache: &Value,
    keyless_enabled: bool,
) -> Option<(String, String)> {
    let key = std::env::var("OPENCODE_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            cfg.pointer("/provider/opencode/options/apiKey")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(String::from)
        })
        .or_else(|| {
            auth.pointer("/opencode/key")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(String::from)
        });
    let base = cache
        .pointer("/opencode/api")
        .and_then(|v| v.as_str())
        .map(String::from)?;
    match key {
        Some(k) => Some((base, k)),
        // keyless free tier: discriminator headers ride in ocserve-llm
        // (composite UA) and ocserve-core (tools array) — FINDINGS P5/P8.
        None if keyless_enabled => Some((base, "public".to_string())),
        None => None,
    }
}

/// B2 (freeze provider.ts:196-201): keyless opencode keeps ONLY free
/// models — paid ones would 401 with the public key.
pub(crate) fn apply_public_tier_filter(
    pid: &str,
    has_key: bool,
    models: &mut BTreeMap<String, Value>,
) {
    if pid == "opencode" && !has_key {
        models.retain(|_, m| {
            m.get("cost")
                .and_then(|c| c.get("input"))
                .and_then(|v| v.as_u64())
                == Some(0)
        });
    }
}

/// True when the provider's served model entry is marked `deprecated`.
/// Reads the same `config_providers.providers` list the route serves, so the
/// default can never pick a model the picker no longer offers.
fn catalog_model_deprecated(config_providers: &Value, pid: &str, mid: &str) -> bool {
    config_providers
        .pointer("/providers")
        .and_then(|v| v.as_array())
        .and_then(|list| {
            list.iter()
                .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(pid))
        })
        .and_then(|e| e.pointer(&format!("/models/{mid}/status")))
        .and_then(|v| v.as_str())
        == Some("deprecated")
}

/// Provider catalog membership (W6's `known` rule, factored shared):
/// `config_providers.providers` LIST of {id, models} (cache-derived) OR the
/// config-file dict `provider.<pid>.models` — both consulted (W6 finding).
pub(crate) fn catalog_contains(
    config_providers: &Value,
    cfg: &Value,
    pid: &str,
    mid: &str,
) -> bool {
    if let Some(list) = config_providers
        .pointer("/providers")
        .and_then(|v| v.as_array())
        && let Some(entry) = list
            .iter()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(pid))
        && let Some(models) = entry.get("models")
    {
        let in_models = models.as_object().is_some_and(|o| o.contains_key(mid))
            || models.as_array().is_some_and(|a| {
                a.iter().any(|m| {
                    m.as_str() == Some(mid) || m.get("id").and_then(|v| v.as_str()) == Some(mid)
                })
            });
        if in_models {
            return true;
        }
    }
    cfg.pointer(&format!("/provider/{pid}/models"))
        .and_then(|v| v.as_object())
        .is_some_and(|o| o.contains_key(mid))
}

/// First catalog model for a provider (config order, deterministic):
/// cache-derived providers list first, then the config-file dict.
pub(crate) fn first_catalog_model(config_providers: &Value, cfg: &Value, pid: &str) -> String {
    if let Some(list) = config_providers
        .pointer("/providers")
        .and_then(|v| v.as_array())
        && let Some(entry) = list
            .iter()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(pid))
    {
        if let Some(o) = entry.pointer("/models").and_then(|v| v.as_object())
            && let Some(k) = o.keys().next()
        {
            return k.clone();
        }
        if let Some(a) = entry.pointer("/models").and_then(|v| v.as_array())
            && let Some(first) = a.first()
        {
            return first
                .as_str()
                .map(String::from)
                .or_else(|| first.get("id").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_default();
        }
    }
    cfg.pointer(&format!("/provider/{pid}/models"))
        .and_then(|v| v.as_object())
        .and_then(|o| o.keys().next())
        .cloned()
        .unwrap_or_default()
}

/// Active key credentials from OUR `credential` table (issue #28). Values
/// follow the 2.x tagged union (`{"type":"key","key":...}`); OAuth entries and
/// unparseable values are skipped. Fail-soft: the registry must build even if
/// the database is locked/absent (fresh install before first write).
fn credential_keys(data_dir: &Path) -> std::collections::HashMap<String, String> {
    let db = ocserve_store::writer::db_path(data_dir);
    if !db.exists() {
        return std::collections::HashMap::new();
    }
    match ocserve_store::credential_list(&db) {
        Ok(list) => list
            .into_iter()
            .filter(|c| c.active)
            .filter_map(|c| {
                let integration = c.integration_id.clone()?;
                let v: Value = serde_json::from_str(&c.value).ok()?;
                if v.get("type").and_then(|t| t.as_str()) == Some("oauth") {
                    return None;
                }
                let key = v.get("key").and_then(|k| k.as_str())?;
                (!key.is_empty()).then(|| (integration, key.to_string()))
            })
            .collect(),
        Err(e) => {
            tracing::warn!("credential store unreadable ({e:#}) — not used as an auth source");
            std::collections::HashMap::new()
        }
    }
}

impl Runtime {
    /// CONSUMING variant for the reload path: a live OOM was measured at
    /// three reloads (clone-all payloads() + mimalloc retention ratcheted
    /// 470→665→749MB then the cap killed dispose) — the reloader moves the
    /// fields instead of duplicating the whole payload set every call.
    pub fn into_payloads(self) -> ocserve_http::Payloads {
        // Publish `config.shell` for the bash tool. This is the single choke
        // point every payload build passes through (boot, file reload, plugin
        // `config` hook derive), so a hot-edited shell is visible to the very
        // next command — upstream reads it at spawn time and does not cache.
        // Without this the tool rendered the description for `$SHELL` and then
        // executed `/bin/sh`, so a command the model was told would work could
        // not find `cargo` (measured 2026-10-27).
        ocserve_store::platform::set_config_shell(
            self.config
                .get("shell")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        );
        let compaction = self
            .config
            .pointer("/compaction")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        ocserve_http::Payloads {
            config: self.config,
            agent: self.agent,
            api_agent: self.api_agent,
            command: self.command,
            config_providers: self.config_providers,
            provider: self.provider,
            console: self.console,
            capabilities: self.capabilities,
            compaction,
            // the once-parsed catalog moves with the payload set (v2 metadata
            // routes read it; re-parsing per request was 100 ms CPU + ~25 MB
            // retention — see Payloads::catalog).
            catalog: std::sync::Arc::new(self.catalog),
        }
    }

    /// data_dir-aware load (W5): the auth OVERLAY ocserve writes lives there
    /// and layers over the read-only legacy auth.json. (Boot + reloader both
    /// call this — there is deliberately no env-default variant.)
    pub fn load_for(data_dir: &std::path::Path) -> Result<Self> {
        // Explicit channels (upstream order): OPENCODE_CONFIG then
        // OPENCODE_CONFIG_CONTENT. A launcher that injects its plugin config
        // this way must win over the on-disk file.
        let env_config_path = std::env::var_os("OPENCODE_CONFIG")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from);
        let env_content = match std::env::var("OPENCODE_CONFIG_CONTENT") {
            Ok(v) => Some(v),
            Err(std::env::VarError::NotUnicode(_)) => {
                tracing::warn!("OPENCODE_CONFIG_CONTENT is not valid UTF-8 — ignored");
                None
            }
            Err(std::env::VarError::NotPresent) => None,
        };
        Self::load_for_with(
            data_dir,
            env_config_path,
            env_content,
            ocserve_store::platform::opencode_config_dir(),
        )
    }

    /// The env inputs are explicit here so a test can prove the WIRING (env →
    /// merged config → plugin list) without racing process-wide `set_var`.
    ///
    /// `config_dir` is explicit for the same reason, and it is load-bearing for
    /// test hygiene: the config directory is now a real env input
    /// (`OPENCODE_CONFIG_DIR` / `XDG_CONFIG_HOME`), so a test that let it
    /// default read the DEVELOPER's `~/.config/opencode` — asserting an exact
    /// plugin list against whatever plugins happen to be installed there
    /// (caught 2026-11-12: two tests failed only on a machine with plugins).
    fn load_for_with(
        data_dir: &std::path::Path,
        env_config_path: Option<std::path::PathBuf>,
        env_content: Option<String>,
        config_dir: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_default();
        // Upstream precedence: OPENCODE_CONFIG_DIR > XDG_CONFIG_HOME/opencode
        // > ~/.config/opencode. A launcher provisioning a dedicated config dir
        // must find its opencode.json/plugins read from the same place.
        let config_path = config_dir
            .map(|d| d.join("opencode.json"))
            .unwrap_or_else(|| Path::new(&home).join(".config/opencode/opencode.json"));
        // K-MODELS: OPENCODE_MODELS_URL/_PATH aware (upstream read precedence)
        let cache_path = crate::models_dev::read_path();

        let mut raw_config = read_global_config(&config_path);
        // W4 overlay: a ocserve-owned patch file (OCSERVE_CONFIG_WRITE=overlay
        // writes it) deep-merges OVER the shared opencode.json on every load —
        // never mutates the user's other server's config unless told to.
        let overlay_path = Path::new(&home).join(".config/ocserve/config.json");
        if overlay_path.exists() {
            let overlay = read_json(&overlay_path)?;
            ocserve_http::deep_merge(&mut raw_config, overlay);
        }
        let base_dir = std::env::current_dir().unwrap_or_else(|_| Path::new(".").into());
        raw_config = layer_env_config(
            raw_config,
            env_config_path,
            env_content.as_deref(),
            &base_dir,
        )?;
        let auth = load_auth(data_dir)?;
        let cache = read_catalog(&cache_path);
        Self::from_parts(data_dir, raw_config, auth, cache)
    }

    /// Build a Runtime from an explicit config value (plugin `config` hook).
    /// Auth + the model catalog are re-read — the same cost as a normal
    /// reload, and it keeps ONE derivation path for both.
    pub fn from_config(data_dir: &std::path::Path, config: Value) -> Result<Self> {
        let auth = load_auth(data_dir)?;
        let cache = read_catalog(&crate::models_dev::read_path());
        Self::from_parts(data_dir, config, auth, cache)
    }

    fn from_parts(
        data_dir: &std::path::Path,
        raw_config: Value,
        auth: Value,
        cache: Value,
    ) -> Result<Self> {
        let (agent, api_agent) = build_agents(&raw_config);
        let command = build_commands(&raw_config)?;
        let (config_providers, provider) = build_providers(&raw_config, &auth, &cache);
        // Pair gate (2026-10-06): freeze's config loader ALWAYS answers
        // agent/command/mode ({} when unset) + username. Applied to the SERVED
        // config only; derivations above use the raw value (unchanged order).
        let mut config = raw_config;
        // `plugin_origins` exists ONLY in the config handed to the plugin
        // (upstream derives it for the hook). It must never become a served
        // payload: it carries an absolute home path and is not a wire key
        // (review M-5).
        if let Some(obj) = config.as_object_mut() {
            obj.remove("plugin_origins");
            for k in ["agent", "command", "mode"] {
                obj.entry(k).or_insert(json!({}));
            }
            if !obj.contains_key("username")
                && let Ok(u) = std::env::var("USER")
                    .or_else(|_| std::env::var("LOGNAME"))
                    .or_else(|_| std::env::var("USERNAME"))
                && !u.is_empty()
            {
                obj.insert("username".into(), json!(u));
            }
        }

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            config,
            catalog: cache,
            agent,
            api_agent,
            command,
            config_providers,
            provider,
            console: json!({"consoleManagedProviders": [], "switchableOrgCount": 0}),
            capabilities: json!({"backgroundSubagents": false}),
        })
    }
}

/// `OPENCODE_ENABLE_EXPERIMENTAL_MODELS` (upstream runtime-flags.ts:40,
/// parsed as an Effect boolean: `true`/`1` are truthy). Read through a
/// OnceLock: process-wide, like the other runtime flags.
pub(crate) fn experimental_models() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            std::env::var("OPENCODE_ENABLE_EXPERIMENTAL_MODELS")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        )
    })
}

/// Upstream `provider.ts:1744-1745`: a served model must be neither
/// `deprecated` nor (`alpha` while experimental models are off). Unknown or
/// missing status counts as active.
pub(crate) fn model_is_offered(model: &Value, experimental: bool) -> bool {
    match model.get("status").and_then(|v| v.as_str()) {
        Some("deprecated") => false,
        Some("alpha") => experimental,
        _ => true,
    }
}

fn build_providers(cfg: &Value, auth: &Value, cache: &Value) -> (Value, Value) {
    let cfg_providers = cfg.get("provider").and_then(|v| v.as_object());
    let auth_obj = auth.as_object();
    let cache_obj = cache.as_object();

    // state provider order: auth.json key order, then built-in opencode, then config-only
    let mut state_order: Vec<String> = auth_obj
        .map(|a| a.keys().cloned().collect())
        .unwrap_or_default();
    if !state_order.iter().any(|p| p == "opencode") {
        state_order.push("opencode".into());
    }
    if let Some(cp) = cfg_providers {
        for pid in cp.keys() {
            if !state_order.contains(pid) {
                state_order.push(pid.clone());
            }
        }
    }

    let source_of = |pid: &str| -> &'static str {
        if cfg_providers.map(|c| c.contains_key(pid)).unwrap_or(false) {
            "config"
        } else if auth_obj.map(|a| a.contains_key(pid)).unwrap_or(false) {
            "api"
        } else {
            "custom"
        }
    };

    // per-provider Info builder (cache base + config overlay + auth key)
    let build_info = |pid: &str| -> Option<Value> {
        let c = cache_obj.and_then(|c| c.get(pid));
        let cb = cfg_providers.and_then(|c| c.get(pid));
        if c.is_none() && cb.is_none() {
            return None;
        }
        let npm = cb
            .and_then(|b| b.get("npm"))
            .and_then(|v| v.as_str())
            .or_else(|| c.and_then(|x| x.get("npm")).and_then(|v| v.as_str()))
            .unwrap_or("");
        let name = cb
            .and_then(|b| b.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| c.and_then(|x| x.get("name")).and_then(|v| v.as_str()))
            .unwrap_or(pid);
        let env = c
            .and_then(|x| x.get("env"))
            .cloned()
            .or_else(|| cb.and_then(|b| b.get("env")).cloned())
            .unwrap_or_else(|| json!([]));
        // models: cache models overlaid by config model overrides; config-only
        // providers use their config models as-is
        let mut models: BTreeMap<String, Value> = c
            .and_then(|x| x.get("models"))
            .and_then(|m| m.as_object())
            .map(sort_json_keys)
            .unwrap_or_default();
        if let Some(cm) = cb.and_then(|b| b.get("models")).and_then(|m| m.as_object()) {
            for (mid, mval) in cm {
                models.insert(mid.clone(), transform_config_model(pid, mid, mval));
            }
        }
        // Freeze `provider.ts:1744-1745`: models marked `deprecated` and
        // `alpha` (unless OPENCODE_ENABLE_EXPERIMENTAL_MODELS) are DELETED
        // from the served model map, not merely hidden in one projection.
        // The catalog carried 37 retired free models for `opencode`; serving
        // them meant the picker offered models that no longer answer (and the
        // retired ones failed with provider errors rather than a clean 404).
        models.retain(|_, m| model_is_offered(m, experimental_models()));
        // B2: single site covers /config/providers + /provider routes.
        let has_key = std::env::var("OPENCODE_API_KEY").is_ok_and(|v| !v.is_empty())
            || auth_obj
                .and_then(|a| a.get(pid))
                .and_then(|v| v.get("key"))
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty())
            || cb
                .and_then(|b| b.pointer("/options/apiKey"))
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty());
        apply_public_tier_filter(pid, has_key, &mut models);
        // re-transform cache models into wire shape (config inserts already transformed)
        let mut wire: serde_json::Map<String, Value> = serde_json::Map::new();
        for (mid, mval) in &models {
            // config-transformed entries carry providerID already; cache ones don't
            if mval.get("providerID").is_some() {
                wire.insert(mid.clone(), mval.clone());
            } else {
                wire.insert(mid.clone(), transform_cache_model(pid, npm, mid, mval));
            }
        }
        let key = auth_obj
            .and_then(|a| a.get(pid))
            .and_then(|v| v.get("key"))
            .and_then(|v| v.as_str());
        let mut info = json!({
            "id": pid,
            "name": name,
            "source": source_of(pid),
            "env": env,
            "options": cb.and_then(|b| b.get("options")).cloned().unwrap_or(json!({})),
            "models": wire,
        });
        if let Some(k) = key {
            info["key"] = json!(k);
        }
        Some(info)
    };

    // ---- /config/providers: state order ----
    let mut state_infos = Vec::new();
    let mut state_defaults = serde_json::Map::new();
    for pid in &state_order {
        if let Some(info) = build_info(pid) {
            let d = default_model(&model_map(&info["models"]));
            if let Some(m) = d {
                state_defaults.insert(pid.clone(), json!(m));
            }
            state_infos.push(info);
        }
    }
    let config_providers = json!({
        "providers": state_infos,
        "default": Value::Object(state_defaults),
    });

    // ---- /provider: cache order overlaid in place, config-only appended ----
    let mut all: Vec<(String, Value)> = Vec::new();
    if let Some(c) = cache_obj {
        for pid in c.keys() {
            if let Some(info) = build_info(pid) {
                all.push((pid.clone(), info));
            }
        }
    }
    for pid in &state_order {
        if all.iter().any(|(id, _)| id == pid) {
            continue; // overlaid in place below
        }
        if let Some(info) = build_info(pid) {
            all.push((pid.clone(), info));
        }
    }
    // overlay state entries (they replace cache entries at the same position)
    for (id, info) in all.iter_mut() {
        if state_order.contains(id)
            && let Some(fresh) = build_info(id)
        {
            *info = fresh;
        }
    }
    let mut defaults = serde_json::Map::new();
    for (pid, info) in &all {
        if let Some(d) = default_model(&model_map(&info["models"])) {
            defaults.insert(pid.clone(), json!(d));
        }
    }
    let connected: Vec<Value> = all
        .iter()
        .filter(|(pid, _)| state_order.contains(pid))
        .map(|(_, info)| info["id"].clone())
        .collect();
    let provider = json!({
        "all": all.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "default": Value::Object(defaults),
        "connected": connected,
    });

    (config_providers, provider)
}

fn build_agents(cfg: &Value) -> (Vec<Value>, Vec<Value>) {
    // Permission defaults ported from v1 agent.ts (user config.permission merged last).
    let home = std::env::var("HOME").unwrap_or_default();
    let data = format!("{home}/.local/share/opencode");
    let defaults = json!({
        "*": "allow",
        "doom_loop": "ask",
        "external_directory": {
            "*": "ask",
            format!("{data}/tool-output/*"): "allow",
            "/tmp/*": "allow",
        },
        "question": "deny",
        "plan_enter": "deny",
        "plan_exit": "deny",
        "read": {"*": "allow", "*.env": "ask", "*.env.*": "ask", "*.env.example": "allow"},
    });
    let user = cfg.get("permission").cloned().unwrap_or(json!({}));

    let flat = |extra: Value| -> Vec<Value> {
        let mut rules = perm_from_config(&defaults);
        if !extra.is_null() {
            rules.extend(perm_from_config(&extra));
        }
        rules.extend(perm_from_config(&user));
        rules
    };

    let mut agents: Vec<Value> = vec![
        json!({
            "name": "build",
            "description": "The default agent. Executes tools based on configured permissions.",
            "options": {},
            "permission": flat(json!({"question": "allow", "plan_enter": "allow"})),
            "mode": "primary",
            "native": true,
        }),
        json!({
            "name": "plan",
            "description": "Plan mode. Disallows all edit tools.",
            "options": {},
            "permission": flat(json!({
                "question": "allow", "plan_exit": "allow",
                "task": {"general": "deny"},
                "edit": {"*": "deny", ".opencode/plans/*.md": "allow"},
            })),
            "mode": "primary",
            "native": true,
        }),
        json!({
            "name": "general",
            "description": "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.",
            "permission": flat(json!({"todowrite": "deny"})),
            "options": {},
            "mode": "subagent",
            "native": true,
        }),
        json!({
            "name": "explore",
            "description": "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.",
            "prompt": include_str!("../assets/explore.txt"),
            "options": {},
            "mode": "subagent",
            "native": true,
            "permission": flat(json!({
                "*": "deny",
                "grep": "allow", "glob": "allow", "list": "allow", "bash": "allow",
                "webfetch": "allow", "websearch": "allow", "read": "allow",
            })),
        }),
        json!({
            "name": "compaction",
            "mode": "primary",
            "native": true,
            "hidden": true,
            "prompt": include_str!("../assets/compaction.txt"),
            "permission": flat(json!({"*": "deny"})),
            "options": {},
        }),
        json!({
            "name": "title",
            "mode": "primary",
            "options": {},
            "native": true,
            "hidden": true,
            "temperature": 0.5,
            "permission": flat(json!({"*": "deny"})),
            "prompt": include_str!("../assets/title.txt"),
        }),
        json!({
            "name": "summary",
            "mode": "primary",
            "options": {},
            "native": true,
            "hidden": true,
            "permission": flat(json!({"*": "deny"})),
            "prompt": include_str!("../assets/summary.txt"),
        }),
    ];

    // config agents overlay (v1 agent.ts loop)
    if let Some(cfg_agents) = cfg.get("agent").and_then(|v| v.as_object()) {
        for (key, value) in cfg_agents {
            if value.get("disable").and_then(|v| v.as_bool()) == Some(true) {
                agents.retain(|a| a["name"].as_str() != Some(key.as_str()));
                continue;
            }
            let existing = agents
                .iter_mut()
                .find(|a| a["name"].as_str() == Some(key.as_str()));
            if let Some(item) = existing {
                merge_agent(item, value);
            } else {
                let mut item = json!({
                    "name": key,
                    "mode": "all",
                    "permission": flat(json!({})),
                    "options": {},
                    "native": false,
                });
                merge_agent(&mut item, value);
                agents.push(item);
            }
        }
    }

    // declaration order is the /api/agent order (natives then config, no sort)
    let declaration_order = agents.clone();

    // sort: default_agent first, then name asc (v1 agent.ts list())
    let default_agent = cfg
        .get("default_agent")
        .and_then(|v| v.as_str())
        .unwrap_or("build");
    agents.sort_by(|a, b| {
        let an = a["name"].as_str().unwrap_or("");
        let bn = b["name"].as_str().unwrap_or("");
        let ad = an == default_agent;
        let bd = bn == default_agent;
        bd.cmp(&ad).then_with(|| an.cmp(bn))
    });
    (agents, declaration_order)
}

fn merge_agent(item: &mut Value, value: &Value) {
    let Some(v) = value.as_object() else { return };
    // permission handled after the object borrow ends (borrowck)
    // PER-AGENT `tools` map: upstream normalizes it into permission rules
    // (core/v1/config/agent.ts normalize: `tools:{bash:false}` → `bash:deny`,
    // and write/edit/patch fold into `edit`). ocserve dropped the map entirely,
    // so a config agent's `tools` had no effect (2026-10-24 review R8).
    // Tools folded BEFORE the explicit permission map, which upstream applies
    // last (Object.assign(permission, agent.permission)).
    let tools_perm = v.get("tools").and_then(|t| t.as_object()).map(|tools| {
        let mut out = serde_json::Map::new();
        for (name, enabled) in tools {
            let action = if enabled.as_bool().unwrap_or(false) {
                "allow"
            } else {
                "deny"
            };
            let key = if name == "write" || name == "edit" || name == "patch" {
                "edit"
            } else {
                name.as_str()
            };
            out.insert(key.to_string(), json!(action));
        }
        Value::Object(out)
    });
    let extra_perm = v.get("permission").cloned();
    let obj = item.as_object_mut().expect("agent object");
    for (k, val) in v {
        // `disable`/`permission` handled specially. `tools` is consumed by the
        // prompt layer and is NOT an Agent response field (upstream agent.ts
        // copies a fixed set; `tools` is absent). `model` is a
        // "provider/model" string in config but an object in the response
        // (upstream `Provider.parseModel`).
        if k == "disable" || k == "permission" || k == "tools" {
            continue;
        }
        if k == "top_p" {
            obj.insert("topP".into(), val.clone());
        } else if k == "model" {
            if let Some(s) = val.as_str()
                && let Some((pid, mid)) = s.split_once('/')
            {
                obj.insert("model".into(), json!({"providerID": pid, "modelID": mid}));
            }
            // a non-string model (already an object) is left untouched
        } else if k == "options" {
            let opts = obj
                .entry("options")
                .or_insert(json!({}))
                .as_object_mut()
                .expect("options");
            if let Some(extra) = val.as_object() {
                for (ok, ov) in extra {
                    opts.insert(ok.clone(), ov.clone());
                }
            }
        } else {
            obj.insert(k.clone(), val.clone());
        }
    }
    if let Some(perm) = extra_perm {
        let mut rules = obj
            .get("permission")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        // tools first, then explicit permission (upstream order: permission wins)
        if let Some(tp) = &tools_perm {
            rules.extend(perm_from_config(tp));
        }
        rules.extend(perm_from_config(&perm));
        obj.insert("permission".into(), Value::Array(rules));
    } else if let Some(tp) = &tools_perm {
        let mut rules = obj
            .get("permission")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        rules.extend(perm_from_config(tp));
        obj.insert("permission".into(), Value::Array(rules));
    }
}

fn build_commands(cfg: &Value) -> Result<Vec<Value>> {
    build_commands_with(cfg, std::env::var("OCSERVE_GOAL").as_deref() != Ok("0"))
}

/// `build_commands` with the goal switch as a PARAMETER: env is process-global,
/// this crate forbids `unsafe`, and `set_var` is unsafe since Rust 2024, so the
/// switch must be injectable for testing.
fn build_commands_with(cfg: &Value, goals: bool) -> Result<Vec<Value>> {
    let init = include_str!("../assets/initialize.txt");
    let review = include_str!("../assets/review.txt");
    // Built-in skill command (freeze parity): v1 skill/index.ts registers
    // `customize-opencode` unconditionally and command/index.ts:134 exposes
    // it through /command with source "skill". Content is the upstream MIT
    // skill body (THIRD-PARTY.md carries the notice).
    let customize = include_str!("../assets/customize-opencode.txt");
    const CUSTOMIZE_DESC: &str = "Use ONLY when the user is editing or creating opencode's own configuration: \
         opencode.json, opencode.jsonc, files under .opencode/, or files under \
         ~/.config/opencode/. Also use when creating or fixing opencode agents, subagents, \
         skills, plugins, MCP servers, or permission rules. Do not use for the user's own \
         application code, or for any project that is not configuring opencode itself.";
    let hints_of = |template: &str| -> Vec<String> {
        let mut hints: Vec<String> = Vec::new();
        let mut rest = template;
        while let Some(idx) = rest.find('$') {
            let tail = &rest[idx..];
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let h = format!("${digits}");
                if !hints.contains(&h) {
                    hints.push(h);
                }
            }
            rest = &tail[1..];
        }
        if template.contains("$ARGUMENTS") && !hints.contains(&"$ARGUMENTS".to_string()) {
            hints.push("$ARGUMENTS".into());
        }
        hints
    };
    let mut cmds = vec![
        json!({
            "name": "init", "description": "guided AGENTS.md setup", "source": "command",
            "template": init, "hints": hints_of(init),
        }),
        json!({
            "name": "review", "description": "Code review", "source": "command",
            "template": review, "hints": hints_of(review),
        }),
        json!({
            "name": "customize-opencode", "description": CUSTOMIZE_DESC, "source": "skill",
            "template": customize, "hints": <Vec<String>>::new(),
        }),
    ];
    // `/goal` is a built-in command (upstream: a command.transform entry from
    // its built-in goal plugin, not a template). It has no template because
    // its argument is state; the command handler branches on the name. Listed
    // here so both surfaces can discover it. A user command named `goal`
    // below replaces it (upstream's transform also lets a user command win).
    if goals
        && !cfg
            .get("command")
            .and_then(|v| v.as_object())
            .is_some_and(|m| m.contains_key("goal"))
    {
        cmds.push(json!({
            "name": "goal",
            "description": "keep working until an objective is met [pause|resume|clear]",
            "source": "command",
            "template": "",
            "hints": <Vec<String>>::new(),
        }));
    }
    if let Some(commands) = cfg.get("command").and_then(|v| v.as_object()) {
        for (name, c) in commands {
            let template = c
                .get("template")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            cmds.push(json!({
                "name": name,
                "description": c.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "source": "command",
                "template": template,
                "hints": hints_of(template),
            }));
        }
    }
    Ok(cmds)
}

impl Runtime {
    /// Assemble the LLM registry from auth/config/models-cache/state
    /// (endpoints, pricing, default model, agent systems).
    pub fn llm_registry(&self) -> Result<LlmRegistry> {
        let home = std::env::var("HOME").unwrap_or_default();
        let read_json = |p: &str| -> Option<Value> {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
        };
        // layered (legacy + overlay) — same rule as load_for; Result so a
        // broken overlay fails the whole reload (fail-safe: keep old registry)
        let auth = load_auth(&self.data_dir)?;
        // One derivation source: the Runtime's OWN parsed config (which the
        // plugin `config` hook may have replaced via `from_config`). Reading
        // the file again here silently discarded hook-mutated providers and
        // duplicated a disk read (issue #28's test exposed it).
        let cfg = self.config.clone();
        // A1: reuse the catalog load_for already parsed (never re-read —
        // double-parsing the 5.3 MB file per reload was the proven spike).
        let cache = &self.catalog;
        let state_model = read_json(&format!("{home}/.local/state/opencode/model.json"))
            .unwrap_or_else(|| json!({}));

        // Known default endpoints for auth'd providers without config baseURL
        // (provider quirks live in code-data, PLAN §8 — extend as needed).
        let defaults: [(&str, &str); 2] = [
            ("deepseek", "https://api.deepseek.com/v1"),
            ("openrouter", "https://openrouter.ai/api/v1"),
        ];

        let mut endpoints = std::collections::HashMap::new();
        if let Some(obj) = auth.as_object() {
            for (pid, entry) in obj {
                let key = entry.get("key").and_then(|k| k.as_str()).unwrap_or("");
                if key.is_empty() {
                    continue; // oauth-typed entries: M2 supports key auth only
                }
                let base = cfg
                    .pointer(&format!("/provider/{pid}/options/baseURL"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    // The models.dev catalog carries the provider's API base
                    // (`api`); opencode-go is auth-only, so the hardcoded
                    // defaults list alone left `opencode auth login` users
                    // with no endpoint and no provider on the v2 wire.
                    .or_else(|| {
                        cache
                            .get(pid)
                            .and_then(|p| p.get("api"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .or_else(|| {
                        defaults
                            .iter()
                            .find(|(d, _)| d == pid)
                            .map(|(_, u)| (*u).to_string())
                    });
                if let Some(base) = base {
                    endpoints.insert(pid.clone(), (base, key.to_string()));
                }
            }
        }
        // config providers without auth entries (baseURL + env-less key)
        if let Some(provs) = cfg.get("provider").and_then(|v| v.as_object()) {
            for (pid, p) in provs {
                if endpoints.contains_key(pid) {
                    continue;
                }
                if let Some(base) = p.pointer("/options/baseURL").and_then(|v| v.as_str()) {
                    // issue #28: auth layer first, then the provider's own
                    // config key. `{env:}`/`{file:}` are already expanded by
                    // the loader, so this is the literal secret here.
                    let key = auth
                        .pointer(&format!("/{pid}/key"))
                        .and_then(|v| v.as_str())
                        .filter(|k| !k.is_empty())
                        .or_else(|| {
                            p.pointer("/options/apiKey")
                                .and_then(|v| v.as_str())
                                .filter(|k| !k.is_empty())
                        })
                        .unwrap_or("");
                    if key.is_empty() {
                        tracing::warn!(
                            "provider {pid} has a baseURL but resolves to an empty API key \
                             (checked auth.json, auth-overlay, credential store, provider.options.apiKey)"
                        );
                    }
                    endpoints.insert(pid.clone(), (base.to_string(), key.to_string()));
                }
            }
        }

        // B2 (K-MODEL-STATE): opencode public-tier endpoint (pure fn below).
        if let Some(pair) = opencode_public_endpoint(&cfg, &auth, cache) {
            endpoints.insert("opencode".into(), pair);
        }

        // Third auth source (issue #28, 2.x): active `credential`-table keys in
        // OUR database, layered over auth.json/auth-overlay. opencode 2.x does
        // not write auth.json, so config-declared providers otherwise had no
        // path to a key at all. `credential_list` is a read-only handle; a
        // read failure is loud but never fails the registry (boot must survive).
        for (pid, key) in credential_keys(&self.data_dir) {
            if let Some(slot) = endpoints.get_mut(&pid) {
                slot.1 = key;
                continue;
            }
            if let Some(base) = cfg
                .pointer(&format!("/provider/{pid}/options/baseURL"))
                .and_then(|v| v.as_str())
            {
                endpoints.insert(pid, (base.to_string(), key));
            }
        }

        // pricing from models cache: (in, out, cache_read) USD per MTok
        let mut pricing = std::collections::HashMap::new();
        let mut limits = std::collections::HashMap::new();
        // Input modalities decide whether an attachment may be sent as media
        // or must become an explanatory text part (upstream `unsupportedParts`).
        let mut modalities: std::collections::HashMap<(String, String), Vec<String>> =
            std::collections::HashMap::new();
        if let Some(provs) = cache.as_object() {
            for (pid, p) in provs {
                if let Some(models) = p.get("models").and_then(|v| v.as_object()) {
                    for (mid, m) in models {
                        limits.entry((pid.clone(), mid.clone())).or_insert_with(|| {
                            m.get("limit").cloned().unwrap_or(serde_json::Value::Null)
                        });
                        if let Some(arr) = m.pointer("/modalities/input").and_then(|v| v.as_array())
                        {
                            modalities.insert(
                                (pid.clone(), mid.clone()),
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect(),
                            );
                        }
                        let c = m.get("cost");
                        if let (Some(i), Some(o)) = (
                            c.and_then(|c| c.get("input")).and_then(|v| v.as_f64()),
                            c.and_then(|c| c.get("output")).and_then(|v| v.as_f64()),
                        ) {
                            let cr = c
                                .and_then(|c| c.get("cache_read"))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0);
                            pricing.insert((pid.clone(), mid.clone()), (i, o, cr));
                        }
                    }
                }
            }
        }
        // config-defined model costs
        if let Some(provs) = cfg.get("provider").and_then(|v| v.as_object()) {
            for (pid, p) in provs {
                if let Some(models) = p.get("models").and_then(|v| v.as_object()) {
                    for (mid, m) in models {
                        limits.entry((pid.clone(), mid.clone())).or_insert_with(|| {
                            m.get("limit").cloned().unwrap_or(serde_json::Value::Null)
                        });
                        // Upstream derives capabilities from `modalities` with
                        // `text` defaulting TRUE and the rest FALSE
                        // (provider.ts:1577-1581); a config model that omits
                        // `modalities` is text-only, so an attachment must be
                        // stripped rather than sent.
                        let input_mods: Vec<String> = m
                            .pointer("/modalities/input")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_else(|| vec!["text".to_string()]);
                        modalities.insert((pid.clone(), mid.clone()), input_mods);
                        if let (Some(i), Some(o)) = (
                            m.pointer("/cost/input").and_then(|v| v.as_f64()),
                            m.pointer("/cost/output").and_then(|v| v.as_f64()),
                        ) {
                            let cr = m
                                .pointer("/cost/cache_read")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0);
                            pricing.insert((pid.clone(), mid.clone()), (i, o, cr));
                        }
                    }
                }
            }
        }

        // endpoint-aware state default (pick_default_model): a state entry
        // whose provider has no endpoint 400s every model-less prompt and
        // poisons the currentModel route the app sends back
        // catalog source for default-pick filtering (same inputs load_for
        // uses — cheap JSON assembly, no I/O)
        let (config_providers, _provider) = build_providers(&cfg, &auth, cache);
        let default_model = pick_default_model(&state_model, &cfg, &config_providers, &endpoints);
        if let Some(r0) = state_model.pointer("/recent/0") {
            let rp = r0.get("providerID").and_then(|v| v.as_str()).unwrap_or("");
            let rm = r0.get("modelID").and_then(|v| v.as_str()).unwrap_or("");
            if !rp.is_empty() && !endpoints.contains_key(rp) {
                tracing::warn!(
                    "state default model {rp}/{rm} has no configured endpoint — \
                     effective default falls back to {}/{}",
                    default_model.0,
                    default_model.1
                );
            }
        }

        // agent systems from the assembled agent list
        let mut systems = std::collections::HashMap::new();
        for a in &self.agent {
            if let (Some(name), Some(prompt)) = (
                a.get("name").and_then(|v| v.as_str()),
                a.get("prompt").and_then(|v| v.as_str()),
            ) {
                systems.insert(name.to_string(), prompt.to_string());
            }
        }

        let default_agent = cfg
            .pointer("/default_agent")
            .and_then(|v| v.as_str())
            .unwrap_or("build")
            .to_string();
        Ok(LlmRegistry {
            endpoints,
            pricing,
            limits,
            modalities,
            default_model,
            systems,
            default_agent,
        })
    }
}

#[cfg(test)]
mod goal_command_tests {
    #[test]
    fn goal_is_advertised_with_no_template() {
        let cmds = super::build_commands_with(&serde_json::json!({}), true).expect("commands");
        let goal = cmds
            .iter()
            .find(|c| c["name"] == "goal")
            .expect("goal advertised");
        // No template: its argument is STATE, and the command handler branches
        // on the name before template lookup.
        assert_eq!(goal["template"], "");
        assert_eq!(goal["source"], "command");
    }

    #[test]
    fn a_user_goal_command_wins() {
        let cmds = super::build_commands_with(
            &serde_json::json!({
                "command": {"goal": {"template": "mine $ARGUMENTS", "description": "user"}}
            }),
            true,
        )
        .expect("commands");
        let goals: Vec<&serde_json::Value> = cmds.iter().filter(|c| c["name"] == "goal").collect();
        assert_eq!(goals.len(), 1, "exactly one goal command");
        assert_eq!(goals[0]["template"], "mine $ARGUMENTS");
    }

    /// A built-in command is advertised with `template: ""` because the v1
    /// `Command` schema requires the field — so empty-template means "handled
    /// by name, not expandable". Every such entry must be one the HTTP
    /// dispatcher actually special-cases; an accidental empty template would
    /// advertise a command that expands to an empty prompt (the 2026-11-05
    /// `/goal` regression). This pins the set so adding one is a deliberate
    /// two-place change.
    #[test]
    fn empty_template_commands_are_the_handled_builtins() {
        let cmds = super::build_commands_with(&serde_json::json!({}), true).expect("commands");
        let empty: Vec<&str> = cmds
            .iter()
            .filter(|c| c["template"].as_str() == Some(""))
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(
            empty,
            vec!["goal"],
            "only the built-in goal command may advertise an empty template"
        );
    }

    #[test]
    fn kill_switch_hides_the_command() {
        let cmds = super::build_commands_with(&serde_json::json!({}), false).expect("commands");
        assert!(
            cmds.iter().all(|c| c["name"] != "goal"),
            "the kill switch hides it"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P4 (2026-10-09): config agents carry `model` as a "provider/model"
    /// string and a `tools` map. Upstream agent.ts parses model via
    /// Provider.parseModel and never copies `tools` into the response — the
    /// live /agent was emitting a string `model` + a `tools` key, both of
    /// which violate the Agent schema (additionalProperties:false).
    #[test]
    fn config_agent_model_parsed_and_tools_dropped() {
        let cfg = serde_json::json!({
            "agent": {
                "analyze": {
                    "mode": "primary",
                    "model": "bailian-coding-plan/qwen3.5-plus",
                    "tools": {"bash": false, "edit": false},
                    "permission": {"bash": "deny"}
                }
            }
        });
        let (agents, _decl) = build_agents(&cfg);
        let a = agents
            .iter()
            .find(|a| a["name"] == "analyze")
            .expect("config agent present");
        assert_eq!(
            a["model"],
            serde_json::json!({"providerID": "bailian-coding-plan", "modelID": "qwen3.5-plus"}),
            "model string parsed to {{providerID, modelID}}"
        );
        assert!(
            a.get("tools").is_none(),
            "tools must not appear in the Agent response"
        );
    }

    /// R8 (2026-10-24): per-agent `tools:{bash:false}` must become a deny rule,
    /// exactly as upstream normalizes it (core/v1/config/agent.ts). The old
    /// code dropped the map, so the setting silently did nothing.
    #[test]
    fn config_agent_tools_map_becomes_permission_rules() {
        let cfg = serde_json::json!({
            "agent": {
                "readonly": {
                    "mode": "primary",
                    "tools": {"bash": false, "write": false, "read": true},
                    "permission": {"bash": "allow"}
                }
            }
        });
        let (agents, _) = build_agents(&cfg);
        let a = agents.iter().find(|a| a["name"] == "readonly").unwrap();
        let rules = a["permission"].as_array().unwrap();
        let bash: Vec<&Value> = rules
            .iter()
            .filter(|r| r["permission"] == "bash" && r["pattern"] == "*")
            .collect();
        assert!(
            bash.iter().any(|r| r["action"] == "deny"),
            "tools.bash=false must contribute a deny rule: {rules:?}"
        );
        // Explicit `permission: {bash: "allow"}` is applied after tools, so it
        // wins (upstream Object.assign order). The LAST matching rule decides.
        let last_bash = bash
            .iter()
            .rev()
            .find(|r| r["action"].is_string())
            .map(|r| r["action"].as_str().unwrap().to_string())
            .unwrap();
        assert_eq!(last_bash, "allow", "explicit permission must win");
        // write folds into edit.
        assert!(
            rules
                .iter()
                .any(|r| r["permission"] == "edit" && r["action"] == "deny"),
            "tools.write=false must fold into edit:deny: {rules:?}"
        );
    }

    /// P4 (2026-10-09): a config model with no `limit` block must still emit
    /// `limit.context`+`limit.output` (freeze defaults both to 0;
    /// provider.ts:1609). The raw block left `{}` → spec violation.
    #[test]
    fn config_model_limit_defaults_to_zero() {
        let m = serde_json::json!({"name": "Free Model"});
        let out = transform_config_model("p", "free-model", &m);
        assert_eq!(out["limit"]["context"], 0);
        assert_eq!(out["limit"]["output"], 0);
        // a full limit passes through
        let m2 = serde_json::json!({"limit": {"context": 1000, "output": 500}});
        let out2 = transform_config_model("p", "m2", &m2);
        assert_eq!(out2["limit"]["context"], 1000);
        assert_eq!(out2["limit"]["output"], 500);
    }

    #[test]
    fn layer_auth_overlay_wins_and_legacy_survives() {
        let legacy = serde_json::json!({
            "keep": "legacy",
            "override": "old",
            "deep": {"a": "1", "b": "2"}
        });
        let overlay = serde_json::json!({
            "override": "new",
            "added": {"key": "k"},
            "deep": {"b": "9"}
        });
        let out = layer_auth(legacy, overlay);
        assert_eq!(out["keep"], "legacy", "legacy-only entry survives");
        assert_eq!(out["override"], "new", "overlay wins on conflict");
        assert_eq!(out["added"]["key"], "k", "overlay-only entry added");
        assert_eq!(out["deep"]["a"], "1", "deep legacy survives");
        assert_eq!(out["deep"]["b"], "9", "overlay wins deep");
    }

    fn endpoints(ids: &[&str]) -> std::collections::HashMap<String, (String, String)> {
        ids.iter()
            .map(|i| (i.to_string(), ("http://x".into(), "k".into())))
            .collect()
    }

    #[test]
    fn served_models_exclude_deprecated_and_alpha() {
        // Freeze provider.ts:1744-1745. The catalog carried 37 retired free
        // models for `opencode`; they must not reach the picker.
        let cache = serde_json::json!({
            "p": {
                "api": "http://x", "npm": "@ai-sdk/openai-compatible", "name": "P",
                "models": {
                    "live": {"id": "live", "name": "Live", "cost": {"input": 0}},
                    "retired": {"id": "retired", "name": "Retired", "status": "deprecated",
                                "cost": {"input": 0}},
                    "alpha-one": {"id": "alpha-one", "name": "A", "status": "alpha",
                                  "cost": {"input": 0}},
                }
            }
        });
        let (_cp, provider) =
            build_providers(&serde_json::json!({}), &serde_json::json!({}), &cache);
        let p = provider["all"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "p")
            .unwrap();
        let ids: Vec<&str> = p["models"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            ids,
            vec!["live"],
            "deprecated/alpha models must not be served"
        );
        // The provider default must not be a model we no longer offer.
        assert_eq!(provider["default"]["p"], "live");
    }

    #[test]
    fn experimental_flag_admits_alpha_but_never_deprecated() {
        // Deprecated is unconditional (upstream deletes it before the alpha
        // check); the experimental flag can only revive alpha.
        assert!(!super::model_is_offered(
            &serde_json::json!({"status": "deprecated"}),
            true
        ));
        assert!(!super::model_is_offered(
            &serde_json::json!({"status": "deprecated"}),
            false
        ));
        assert!(super::model_is_offered(
            &serde_json::json!({"status": "alpha"}),
            true
        ));
        assert!(!super::model_is_offered(
            &serde_json::json!({"status": "alpha"}),
            false
        ));
        assert!(super::model_is_offered(&serde_json::json!({}), false));
    }

    #[test]
    fn default_model_skips_a_deprecated_state_entry() {
        let state = serde_json::json!({
            "recent": [
                {"providerID": "p", "modelID": "retired"},
                {"providerID": "p", "modelID": "live"},
            ]
        });
        let config_providers = serde_json::json!({
            "providers": [{
                "id": "p",
                "models": {
                    "retired": {"id": "retired", "status": "deprecated"},
                    "live": {"id": "live", "status": "active"},
                }
            }]
        });
        let (p, m) = pick_default_model(
            &state,
            &serde_json::json!({}),
            &config_providers,
            &endpoints(&["p"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("p", "live"));
    }

    #[test]
    fn default_model_skips_endpointless_state_entries() {
        // THE bug (live 2026-10-04): state recent[0] = opencode/big-pickle
        // with no `opencode` endpoint → every model-less prompt 400'd and
        // currentModel fed the app the same dead model.
        let state = serde_json::json!({
            "recent": [
                {"providerID": "opencode", "modelID": "big-pickle"},
                {"providerID": "opencode-go", "modelID": "mimo-v2.6-flash"}
            ]
        });
        let eps = endpoints(&["opencode-go"]);
        let cp = serde_json::json!({
            "providers": [{"id": "opencode-go", "models": {"mimo-v2.6-flash": {}}}]
        });
        assert_eq!(
            pick_default_model(&state, &serde_json::json!({}), &cp, &eps),
            ("opencode-go".to_string(), "mimo-v2.6-flash".to_string()),
            "first SERVABLE state entry wins"
        );
    }

    /// The port's payoff (K-MODEL-STATE): keyless zen endpoint installs →
    /// state recent[0] = opencode/big-pickle is SERVABLE → default by
    /// construction (user-confirmed target). Companion to the skip test
    /// above: same state, endpoint present instead of absent.
    #[test]
    fn keyless_zen_endpoint_makes_big_pickle_servable_default() {
        let state = serde_json::json!({
            "recent": [{"providerID": "opencode", "modelID": "big-pickle"}]
        });
        let cache = serde_json::json!({"opencode": {"api": "https://opencode.ai/zen/v1"}});
        let (base, key) = opencode_public_endpoint_inner(
            &serde_json::json!({}),
            &serde_json::json!({}),
            &cache,
            true,
        )
        .expect("keyless zen installs");
        assert_eq!(key, "public");
        let mut eps = endpoints(&["opencode"]);
        eps.insert("opencode".to_string(), (base, key));
        let cp = serde_json::json!({
            "providers": [{"id": "opencode", "models": {"big-pickle": {}}}]
        });
        assert_eq!(
            pick_default_model(&state, &serde_json::json!({}), &cp, &eps),
            ("opencode".to_string(), "big-pickle".to_string()),
            "keyless endpoint present → recent[0] big-pickle is the default"
        );
    }

    #[test]
    fn default_model_skips_uncatalogued_state_entries() {
        // the first fix's miss: openrouter HAS an endpoint but
        // stealth/ox-alpha is a known-dead model absent from the catalog
        // (W6 warned in the same boot) — must be skipped for the next
        // endpoint+catalog hit.
        let state = serde_json::json!({
            "recent": [
                {"providerID": "opencode", "modelID": "big-pickle"},
                {"providerID": "openrouter", "modelID": "stealth/ox-alpha"},
                {"providerID": "google", "modelID": "gemini-3.1-pro-preview"}
            ]
        });
        let eps = endpoints(&["openrouter", "google"]);
        let cp = serde_json::json!({
            "providers": [{"id": "google", "models": {"gemini-3.1-pro-preview": {}}}]
        });
        assert_eq!(
            pick_default_model(&state, &serde_json::json!({}), &cp, &eps),
            ("google".to_string(), "gemini-3.1-pro-preview".to_string()),
            "endpoint-ful but uncatalogued entries are skipped"
        );
    }

    #[test]
    fn default_model_falls_back_deterministically() {
        let state = serde_json::json!({
            "recent": [{"providerID": "opencode", "modelID": "big-pickle"}]
        });
        let cfg = serde_json::json!({
            "provider": {"zed": {"models": {"z-1": {}, "z-2": {}}}, "alpha": {"models": {"a-1": {}}}}
        });
        let eps = endpoints(&["zeta", "alpha"]); // HashMap order would be random
        assert_eq!(
            pick_default_model(&state, &cfg, &serde_json::json!({}), &eps),
            ("alpha".to_string(), "a-1".to_string()),
            "lexicographically-first endpoint + its first config model"
        );
    }

    #[test]
    fn default_model_empty_without_endpoints() {
        let state = serde_json::json!({"recent": [{"providerID": "x", "modelID": "y"}]});
        assert_eq!(
            pick_default_model(
                &state,
                &serde_json::json!({}),
                &serde_json::json!({}),
                &endpoints(&[])
            ),
            (String::new(), String::new())
        );
    }

    #[test]
    fn watch_paths_cover_every_runtime_read() {
        let dp = std::path::Path::new("/tmp/ocserve-data");
        // A neutral root, not a literal home path (the repo gate rejects those).
        let cfg = std::path::PathBuf::from("/srv/root/.config/opencode/opencode.json");
        let paths = watch_paths_with(dp, None, cfg.clone());
        assert_eq!(
            paths.len(),
            6,
            "opencode.json, overlay, auth, auth-overlay, models cache, state model"
        );
        assert_eq!(
            paths[0], cfg,
            "shared config first (OPENCODE_CONFIG_DIR-aware): {:?}",
            paths[0]
        );
        assert_eq!(paths[3], dp.join("auth-overlay.json"), "data-dir overlay");
        assert!(
            paths[5].ends_with(".local/state/opencode/model.json"),
            "state model (default-model hot-swap): {:?}",
            paths[5]
        );
        // An explicit OPENCODE_CONFIG is a read input and must be watched.
        let explicit = std::path::PathBuf::from("/tmp/explicit-opencode.json");
        let with_env = watch_paths_with(dp, Some(explicit.clone()), cfg);
        assert_eq!(with_env.len(), 7);
        assert_eq!(with_env[6], explicit);
    }

    #[test]
    fn default_model_priority_rules() {
        let mut models = BTreeMap::new();
        for id in [
            "glm-5",
            "gpt-5-mini",
            "claude-sonnet-4-20250514",
            "some-latest-model",
            "zzz-latest",
        ] {
            models.insert(id.to_string(), json!({}));
        }
        // priority wins over "latest" wins over id desc:
        // claude-sonnet-4 (priority idx1) beats gpt-5-mini (idx0)? — idx in list:
        // gpt-5=0, claude-sonnet-4=1, big-pickle=2, gemini-3-pro=3 → earlier = higher rank
        let d = default_model(&models).unwrap();
        assert_eq!(d, "claude-sonnet-4-20250514", "priority order: {d}");
    }

    #[test]
    fn default_model_latest_and_desc() {
        let mut models = BTreeMap::new();
        for id in ["aaa", "bbb-latest", "zzz"] {
            models.insert(id.to_string(), json!({}));
        }
        // no priority hits; latest asc → bbb-latest first
        assert_eq!(default_model(&models).unwrap(), "bbb-latest");
        let mut models = BTreeMap::new();
        for id in ["aaa", "zzz", "mmm"] {
            models.insert(id.to_string(), json!({}));
        }
        // no priority, no latest → id desc → zzz
        assert_eq!(default_model(&models).unwrap(), "zzz");
    }

    #[test]
    fn perm_flatten_shapes() {
        let cfg = json!({"*": "allow", "read": {"*.env": "deny"}});
        let rules = perm_from_config(&cfg);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["permission"], "*");
        assert_eq!(rules[1]["pattern"], "*.env");
        assert_eq!(rules[1]["action"], "deny");
    }

    #[test]
    fn transform_cache_model_has_wire_keys() {
        let m = json!({
            "id": "t/H", "name": "H", "family": "H",
            "attachment": false, "reasoning": true, "tool_call": true,
            "temperature": true, "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "limit": {"context": 1000}, "cost": {"input": 1.0, "output": 2.0, "cache_read": 0.5}
        });
        let w = transform_cache_model("pid", "npm", "t/H", &m);
        for k in [
            "id",
            "providerID",
            "api",
            "name",
            "family",
            "capabilities",
            "cost",
            "limit",
            "status",
            "options",
            "headers",
            "release_date",
            "variants",
        ] {
            assert!(w.get(k).is_some(), "missing {k}");
        }
        assert_eq!(w["capabilities"]["input"]["text"], true);
        assert_eq!(w["capabilities"]["toolcall"], true);
        assert_eq!(w["cost"]["cache"]["read"], 0.5);
        assert_eq!(w["api"]["npm"], "npm");
    }
}

#[cfg(test)]
mod b2_public_tier_tests {
    use super::*;

    #[test]
    fn opencode_endpoint_keyless_public_by_default_and_kill_switch_reverts() {
        let cfg = json!({});
        let auth = json!({});
        let cache = json!({"opencode": {"api": "https://opencode.ai/zen/v1"}});
        assert_eq!(
            opencode_public_endpoint_inner(&cfg, &auth, &cache, true),
            Some(("https://opencode.ai/zen/v1".into(), "public".into())),
            "keyless zen installs (FINDINGS P5: exact ocserve wire completes 200)"
        );
        assert_eq!(
            opencode_public_endpoint_inner(&cfg, &auth, &cache, false),
            None,
            "OCSERVE_ZEN_KEYLESS=0 restores the pre-port no-endpoint behavior"
        );
        let cfg_key = json!({"provider": {"opencode": {"options": {"apiKey": "real-key"}}}});
        assert_eq!(
            opencode_public_endpoint_inner(&cfg_key, &auth, &cache, true),
            Some(("https://opencode.ai/zen/v1".into(), "real-key".into())),
            "real key still wins over keyless"
        );
    }

    #[test]
    fn opencode_endpoint_prefers_config_then_auth_key() {
        let cache = json!({"opencode": {"api": "https://opencode.ai/zen/v1"}});
        let cfg = json!({"provider": {"opencode": {"options": {"apiKey": "cfg-key"}}}});
        let auth = json!({"opencode": {"key": "auth-key"}});
        assert_eq!(
            opencode_public_endpoint(&cfg, &auth, &cache).unwrap().1,
            "cfg-key",
            "config key beats auth"
        );
        let cfg2 = json!({});
        assert_eq!(
            opencode_public_endpoint(&cfg2, &auth, &cache).unwrap().1,
            "auth-key"
        );
        // no cache api → no endpoint (nothing invented)
        assert_eq!(opencode_public_endpoint(&cfg2, &auth, &json!({})), None);
    }

    #[test]
    fn public_tier_filter_drops_paid_models_only_when_keyless() {
        let mut models: BTreeMap<String, Value> = BTreeMap::new();
        models.insert(
            "big-pickle".into(),
            json!({"cost": {"input": 0, "output": 0}}),
        );
        models.insert(
            "ling-pro".into(),
            json!({"cost": {"input": 3.0, "output": 12.0}}),
        );
        apply_public_tier_filter("opencode", false, &mut models);
        assert_eq!(models.len(), 1, "paid dropped keyless: {models:?}");
        assert!(models.contains_key("big-pickle"));

        // auth'd → both kept; other pids never filtered
        let mut models: BTreeMap<String, Value> = BTreeMap::new();
        models.insert("big-pickle".into(), json!({"cost": {"input": 0}}));
        models.insert("ling-pro".into(), json!({"cost": {"input": 3.0}}));
        apply_public_tier_filter("opencode", true, &mut models);
        assert_eq!(models.len(), 2, "auth'd keeps paid");
        let mut other: BTreeMap<String, Value> = BTreeMap::new();
        other.insert("paid".into(), json!({"cost": {"input": 9.0}}));
        apply_public_tier_filter("deepseek", false, &mut other);
        assert_eq!(other.len(), 1, "non-opencode providers never filtered");
    }
}

#[cfg(test)]
mod bare_machine_tests {
    use super::*;

    /// CI 2026-10-08: `serve` on a bare machine (no ~/.config/opencode) died
    /// at boot with "load runtime config" — a fresh install must start with
    /// defaults (upstream parity: config.ts logs "using defaults" and
    /// continues). The regression this guards: read_json(config)? was fatal.
    #[test]
    fn missing_global_config_reads_as_defaults_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("opencode.json");
        assert_eq!(read_global_config(&missing), json!({}));
    }

    #[test]
    fn corrupt_global_config_reads_as_defaults_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("opencode.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert_eq!(read_global_config(&bad), json!({}), "warn + defaults");
        assert!(bad.exists(), "server never deletes the user's config file");
    }

    #[test]
    fn valid_global_config_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("opencode.json");
        std::fs::write(&good, r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(read_global_config(&good)["theme"], "dark");
    }
}

#[cfg(test)]
mod config_variable_tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// Issue #28: `{env:VAR}` in the RAW config text is expanded before parse
    /// (upstream config/variable.ts:33). The reported symptom was a config
    /// provider reaching the upstream with `Authorization: Bearer ` because
    /// the token was never expanded.
    #[test]
    fn env_substitution_expands_before_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("opencode.json");
        // Tests may mutate process env (the crate forbids unsafe only outside
        // cfg(test)); the variable name is unique to this test.
        unsafe { std::env::set_var("OCSERVE_TEST_FAKE_KEY", "secret-from-env") };
        std::fs::write(
            &p,
            r#"{"provider":{"fakeprov":{"options":{"baseURL":"http://127.0.0.1:4999/v1","apiKey":"{env:OCSERVE_TEST_FAKE_KEY}"}}}}"#,
        )
        .unwrap();
        let cfg = read_global_config(&p);
        assert_eq!(
            cfg.pointer("/provider/fakeprov/options/apiKey")
                .and_then(|v| v.as_str()),
            Some("secret-from-env"),
            "the raw text must be expanded before serde parses it"
        );
    }

    /// Literal keys pass through untouched (the issue's control: it is not
    /// only about `{env:}`).
    #[test]
    fn literal_keys_survive_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            r#"{"provider":{"p":{"options":{"apiKey":"secret-literal"}}}}"#,
        )
        .unwrap();
        let cfg = read_global_config(&p);
        assert_eq!(
            cfg.pointer("/provider/p/options/apiKey")
                .and_then(|v| v.as_str()),
            Some("secret-literal")
        );
    }

    /// `{file:path}` is resolved relative to the CONFIG's directory and the
    /// content is JSON-escaped, so quotes/newlines survive the parse (upstream
    /// JSON.stringify(content.trim()).slice(1,-1)).
    #[test]
    fn file_substitution_is_relative_and_escaped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("key.txt"), "  line1\"quoted\"\nline2  ").unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            r#"{"provider":{"p":{"options":{"apiKey":"{file:key.txt}"}}}}"#,
        )
        .unwrap();
        let cfg = read_global_config(&p);
        assert_eq!(
            cfg.pointer("/provider/p/options/apiKey")
                .and_then(|v| v.as_str()),
            Some("line1\"quoted\"\nline2"),
            "trimmed, JSON-escaped file content"
        );
    }

    /// Negative control that proves the expansion is load-bearing: the SAME
    /// bytes parsed without expansion resolve to the literal token, which is
    /// exactly the empty-Bearer symptom when the token is `{env:…}`.
    #[test]
    fn without_expansion_the_token_is_the_raw_placeholder() {
        let raw = r#"{"apiKey":"{env:OCSERVE_TEST_FAKE_KEY}"}"#;
        let unexpanded: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            unexpanded.get("apiKey").and_then(|v| v.as_str()),
            Some("{env:OCSERVE_TEST_FAKE_KEY}")
        );
        let expanded = expand_config_vars(
            raw,
            Path::new("."),
            &env_of(&[("OCSERVE_TEST_FAKE_KEY", "secret-from-env")]),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&expanded).unwrap();
        assert_eq!(
            v.get("apiKey").and_then(|v| v.as_str()),
            Some("secret-from-env")
        );
    }

    /// A bad `{file:}` reference degrades to defaults loudly (same contract as
    /// a corrupt config: the drop-in boots; the reference is named). Named
    /// divergence D-CONFIG-VAR — upstream aborts the load.
    #[test]
    fn bad_file_reference_degrades_to_defaults_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            r#"{"provider":{"p":{"options":{"apiKey":"{file:missing.txt}"}}}}"#,
        )
        .unwrap();
        assert_eq!(read_global_config(&p), json!({}));
        assert!(p.exists(), "never deletes the user's config");
    }

    /// `~/` expands to HOME (upstream variable.ts:44).
    #[test]
    fn home_relative_file_reference_expands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("k"), "homed").unwrap();
        let out = expand_config_vars("{file:~/k-not-here}", dir.path(), &|_| None);
        // unreadable -> Err naming the resolved HOME path
        let err = out.unwrap_err();
        assert!(err.contains("bad file reference"), "{err}");
    }
}

#[cfg(test)]
mod provider_key_tests {
    use super::*;

    /// Issue #28 root cause at the endpoint layer: a config provider with a
    /// baseURL and `options.apiKey` and NO auth-layer entry used to be built
    /// with `("", …)`. The registry must carry the config key.
    #[test]
    fn config_provider_key_reaches_the_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = json!({
            "provider": {"fakeprov": {
                "npm": "@ai-sdk/openai-compatible",
                "options": {"baseURL": "http://127.0.0.1:4999/v1", "apiKey": "secret-literal"},
                "models": {"m1": {"name": "M1"}}
            }}
        });
        let rt = Runtime::from_parts(dir.path(), cfg, json!({}), json!({})).unwrap();
        let reg = rt.llm_registry().unwrap();
        let (base, key) = reg.endpoints.get("fakeprov").expect("endpoint present");
        assert_eq!(base, "http://127.0.0.1:4999/v1");
        assert_eq!(key, "secret-literal", "config options.apiKey must be used");
    }

    /// Auth-layer keys still win over the config key (overlay > file > config).
    #[test]
    fn auth_layer_key_wins_over_config_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = json!({
            "provider": {"p": {"options": {"baseURL": "http://x.invalid/v1", "apiKey": "from-config"}}}
        });
        // llm_registry layers auth via load_auth(data_dir): auth.json + the
        // overlay in the data dir. Use the real path, not a struct field.
        std::fs::write(
            dir.path().join("auth-overlay.json"),
            r#"{"p":{"type":"api","key":"from-auth"}}"#,
        )
        .unwrap();
        let rt = Runtime::from_parts(dir.path(), cfg, json!({}), json!({})).unwrap();
        let reg = rt.llm_registry().unwrap();
        assert_eq!(reg.endpoints.get("p").unwrap().1, "from-auth");
    }

    /// Third source (2.x): an ACTIVE key credential in OUR database feeds the
    /// provider, because opencode 2.x no longer writes auth.json.
    #[test]
    fn active_credential_table_key_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let db = ocserve_store::writer::db_path(dir.path());
        let w = ocserve_store::Writer::spawn(db.clone()).unwrap();
        ocserve_store::credential_create(
            &w,
            "cred_test",
            Some("fakeprov"),
            "default",
            &json!({"type": "key", "key": "from-credential-table"}).to_string(),
            true,
            1,
        )
        .unwrap();
        let cfg =
            json!({"provider": {"fakeprov": {"options": {"baseURL": "http://127.0.0.1:4999/v1"}}}});
        let rt = Runtime::from_parts(dir.path(), cfg, json!({}), json!({})).unwrap();
        let reg = rt.llm_registry().unwrap();
        assert_eq!(
            reg.endpoints.get("fakeprov").map(|(_, k)| k.as_str()),
            Some("from-credential-table")
        );
    }

    /// OAuth-typed credentials are not bearer keys and must be skipped.
    #[test]
    fn oauth_credential_is_not_treated_as_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = ocserve_store::writer::db_path(dir.path());
        let w = ocserve_store::Writer::spawn(db.clone()).unwrap();
        ocserve_store::credential_create(
            &w,
            "cred_oauth",
            Some("fakeprov"),
            "default",
            &json!({"type": "oauth", "access": "a", "refresh": "r", "expires": 0}).to_string(),
            true,
            1,
        )
        .unwrap();
        let keys = credential_keys(dir.path());
        assert!(keys.is_empty(), "oauth entries are skipped");
    }

    /// The exact reported shape end-to-end at the registry boundary:
    /// `{env:VAR}` in config, env set, no auth file, no overlay.
    #[test]
    fn env_token_from_config_reaches_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("OCSERVE_TEST_ISSUE28", "secret-from-env") };
        let p = dir.path().join("opencode.json");
        std::fs::write(
            &p,
            r#"{"provider":{"fakeprov":{"options":{"baseURL":"http://127.0.0.1:4999/v1","apiKey":"{env:OCSERVE_TEST_ISSUE28}"}}}}"#,
        )
        .unwrap();
        let cfg = read_global_config(&p);
        let rt = Runtime::from_parts(dir.path(), cfg, json!({}), json!({})).unwrap();
        let reg = rt.llm_registry().unwrap();
        assert_eq!(
            reg.endpoints.get("fakeprov").map(|(_, k)| k.as_str()),
            Some("secret-from-env")
        );
    }
}

#[cfg(test)]
mod catalog_selfheal_tests {
    use super::*;

    /// TESTING §1.6 class (2026-10-05): load_for did `read_json(cache)?` —
    /// a corrupt/unreadable models catalog FAILED BOOT, where upstream's
    /// models-dev.ts loadFromDisk deletes the file and refetches. The read
    /// must be fail-open + self-healing, never fatal.
    #[test]
    fn corrupt_models_catalog_self_heals_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("models.json");
        std::fs::write(&corrupt, "{ definitely not json").unwrap();
        let v = read_catalog(&corrupt);
        assert_eq!(v, json!({}), "corrupt catalog reads as empty, not Err");
        assert!(
            !corrupt.exists(),
            "corrupt file removed so the hourly refresh refetches (upstream parity)"
        );

        // valid file: parsed and KEPT (no over-eager deletion)
        let valid = dir.path().join("models2.json");
        std::fs::write(&valid, r#"{"deepseek": {"name": "DeepSeek"}}"#).unwrap();
        let v2 = read_catalog(&valid);
        assert_eq!(
            v2.pointer("/deepseek/name").and_then(|x| x.as_str()),
            Some("DeepSeek")
        );
        assert!(valid.exists(), "healthy catalog must survive");

        // missing file: empty, no error, nothing to remove
        let missing = dir.path().join("nope.json");
        assert_eq!(read_catalog(&missing), json!({}));
    }
}

#[cfg(test)]
mod config_hook_tests {
    use super::*;

    /// `plugin_origins` is hook INPUT only. It carries an absolute home path
    /// and is not a wire key; `from_config` is what turns a hook result into
    /// served payloads, so it must strip it (2026-10-23 security review M-5).
    #[test]
    fn plugin_origins_is_stripped_from_served_config() {
        let dir = std::env::temp_dir().join(format!(
            "ocserve-origins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = json!({
            "plugin_origins": [{"spec": "x", "source": "/var/tmp/opencode/.config/opencode", "scope": "global"}],
            "model": "p/m",
        });
        let rt = Runtime::from_config(&dir, raw).expect("derive");
        assert!(
            rt.config.get("plugin_origins").is_none(),
            "hook-only key leaked into the served payload: {}",
            rt.config
        );
        for k in ["agent", "command", "mode"] {
            assert!(rt.config.get(k).is_some(), "missing pair-gate key {k}");
        }
    }
}

#[cfg(test)]
mod registry_catalog_tests {
    use super::*;

    /// A1 (2026-10-05 OOM postmortem): llm_registry must reuse the catalog
    /// load_for already parsed. The fixture API differs from disk on purpose
    /// — if registry re-read models.json it would return models.opencode.ai
    /// and this test goes red.
    #[test]
    fn llm_registry_uses_stored_catalog_not_a_second_disk_read() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime {
            data_dir: dir.path().to_path_buf(),
            config: json!({}),
            catalog: json!({"opencode": {"api": "https://from-stored.invalid"}}),
            agent: vec![],
            api_agent: vec![],
            command: vec![],
            config_providers: json!({}),
            provider: json!({}),
            console: json!({}),
            capabilities: json!({}),
        };
        let reg = rt.llm_registry().expect("registry builds");
        let ep = reg
            .endpoints
            .get("opencode")
            .expect("keyless endpoint installs");
        assert_eq!(ep.0, "https://from-stored.invalid", "stored catalog wins");
        assert_eq!(ep.1, "public", "keyless default");
    }
}

#[cfg(test)]
mod env_config_tests {
    use super::*;

    /// The Paseo regression (2026-11-09): a launcher injects its plugin tuple
    /// through OPENCODE_CONFIG_CONTENT. It must reach the merged config — and
    /// it must win over the on-disk config (upstream merges it last, local).
    #[test]
    fn content_is_merged_last_and_wins() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"{"plugin":[["file:///tmp/paseo-bridge.mjs",{"baseUrl":"http://127.0.0.1:6767","token":"t"}]],"model":"x/y"}"#;
        let cfg = layer_env_config(
            json!({"model": "stale/old", "theme": "dark"}),
            None,
            Some(content),
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg["model"], "x/y", "explicit content wins over the file");
        assert_eq!(cfg["theme"], "dark", "unrelated file keys survive");
        assert_eq!(
            cfg["plugin"][0][0], "file:///tmp/paseo-bridge.mjs",
            "the plugin tuple reaches the config the loader reads"
        );
    }

    /// Plugin declarations accumulate across layers (upstream mergePluginOrigins):
    /// a launcher's bridge must be ADDED, not replace the plugins the user
    /// already declared in opencode.json. Same for the v2 `plugins` key.
    #[test]
    fn plugin_declarations_accumulate_across_layers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = layer_env_config(
            json!({"plugin": ["from-file"], "plugins": [{"package": "v2-from-file"}]}),
            None,
            Some(r#"{"plugin":["from-content"],"plugins":[{"package":"v2-from-content"}]}"#),
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg["plugin"], json!(["from-file", "from-content"]));
        assert_eq!(
            cfg["plugins"],
            json!([{"package": "v2-from-file"}, {"package": "v2-from-content"}])
        );
    }

    /// Anti-theater wiring test: the env inputs must reach the PLUGIN LIST, not
    /// just the merge helper. This is the exact Paseo path (file:// tuple in
    /// OPENCODE_CONFIG_CONTENT). Deleting the `layer_env_config` call in
    /// load_for_with makes this red.
    #[test]
    fn load_for_with_env_reaches_the_plugin_list() {
        let dir = tempfile::tempdir().unwrap();
        let empty_cfg = dir.path().join("cfgdir");
        std::fs::create_dir_all(&empty_cfg).unwrap();
        let rt = Runtime::load_for_with(
            dir.path(),
            None,
            Some(
                r#"{"plugin":[["file:///tmp/paseo-bridge.mjs",{"baseUrl":"http://127.0.0.1:6767","token":"t"}]]}"#
                    .into(),
            ),
            Some(empty_cfg),
        )
        .unwrap();
        let specs = crate::plugin_specs(&rt.config);
        assert_eq!(specs.len(), 1, "{specs:?}");
        assert_eq!(specs[0].0, "file:///tmp/paseo-bridge.mjs");
        assert_eq!(specs[0].1["baseUrl"], "http://127.0.0.1:6767");
    }

    /// OPENCODE_CONFIG (file) and OPENCODE_CONFIG_CONTENT (inline) layer in
    /// upstream order: scalars from content win, plugin lists from both survive.
    #[test]
    fn explicit_file_and_content_layer_in_upstream_order() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("explicit.json");
        std::fs::write(&file, r#"{"model":"from/file","plugin":["file-plugin"]}"#).unwrap();
        let empty_cfg = dir.path().join("cfgdir");
        std::fs::create_dir_all(&empty_cfg).unwrap();
        let rt = Runtime::load_for_with(
            dir.path(),
            Some(file),
            Some(r#"{"model":"from/content","plugin":["content-plugin"]}"#.into()),
            Some(empty_cfg),
        )
        .unwrap();
        assert_eq!(rt.config["model"], "from/content");
        let specs = crate::plugin_specs(&rt.config);
        let names: Vec<&str> = specs.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(names, vec!["file-plugin", "content-plugin"]);
    }

    /// Negative control for the silent-drop class: malformed injected config
    /// is a loud error, not an ignored variable. (Old behavior: any value of
    /// OPENCODE_CONFIG_CONTENT was never read at all.)
    #[test]
    fn corrupt_content_is_a_loud_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = layer_env_config(json!({}), None, Some("{ not json"), dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("OPENCODE_CONFIG_CONTENT"),
            "{err:#}"
        );
        let err = layer_env_config(json!({}), None, Some("[]"), dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("JSON object"), "{err:#}");
    }

    /// OPENCODE_CONFIG file: missing warns + contributes nothing (upstream
    /// readConfigFile returns undefined); a valid file merges; corrupt is fatal.
    #[test]
    fn explicit_config_file_layers_and_missing_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("explicit.json");
        std::fs::write(&file, r#"{"models":{"p":{"m":{}}}}"#).unwrap();
        let cfg = layer_env_config(
            json!({"theme": "dark"}),
            Some(file.clone()),
            None,
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg["theme"], "dark");
        assert!(cfg.pointer("/models/p/m").is_some());

        let missing = dir.path().join("nope.json");
        let cfg =
            layer_env_config(json!({"theme": "dark"}), Some(missing), None, dir.path()).unwrap();
        assert_eq!(cfg["theme"], "dark", "missing file changes nothing");

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{").unwrap();
        assert!(
            layer_env_config(json!({}), Some(bad), None, dir.path()).is_err(),
            "a corrupt explicit config must fail loudly"
        );
    }
}
