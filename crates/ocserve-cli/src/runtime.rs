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

use anyhow::{Context, Result};
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

/// Global config read with upstream parity (config.ts: missing file → empty,
/// unreadable/corrupt → "using defaults" + continue). Found by CI 2026-10-08:
/// `serve` on a bare machine (no `~/.config/opencode/opencode.json`) died at
/// boot — a fresh install of the drop-in server must start with defaults, and
/// upstream treats both cases as defaults, never as fatal. Per-user errors
/// stay loud: the missing/corrupt file is logged with its path.
fn read_global_config(path: &Path) -> Value {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "global config unreadable ({}), using defaults: {e}",
                    path.display()
                );
                json!({})
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no global config at {} — using defaults", path.display());
            json!({})
        }
        Err(e) => {
            tracing::warn!(
                "global config unreadable ({}), using defaults: {e}",
                path.display()
            );
            json!({})
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
    let home = std::env::var("HOME").unwrap_or_default();
    let h = Path::new(&home);
    vec![
        h.join(".config/opencode/opencode.json"),
        h.join(".config/ocserve/config.json"),
        h.join(".local/share/opencode/auth.json"),
        data_dir.join("auth-overlay.json"),
        // K-MODELS: the catalog ocserve READS (source-aware + fixture flag)
        crate::models_dev::read_path(),
        h.join(".local/state/opencode/model.json"),
    ]
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
    let limit = m.get("limit").cloned().unwrap_or(json!({}));
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
            "input": {"text": true, "audio": false, "image": false, "video": false, "pdf": false},
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
    if let Some(hit) = candidates
        .iter()
        .find(|(p, m)| endpoints.contains_key(p) && catalog_contains(config_providers, cfg, p, m))
    {
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

impl Runtime {
    /// CONSUMING variant for the reload path: a live OOM was measured at
    /// three reloads (clone-all payloads() + mimalloc retention ratcheted
    /// 470→665→749MB then the cap killed dispose) — the reloader moves the
    /// fields instead of duplicating the whole payload set every call.
    pub fn into_payloads(self) -> ocserve_http::Payloads {
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
        }
    }

    /// data_dir-aware load (W5): the auth OVERLAY ocserve writes lives there
    /// and layers over the read-only legacy auth.json. (Boot + reloader both
    /// call this — there is deliberately no env-default variant.)
    pub fn load_for(data_dir: &std::path::Path) -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_default();
        let config_path = Path::new(&home).join(".config/opencode/opencode.json");
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
        let auth = load_auth(data_dir)?;
        let cache = read_catalog(&cache_path);

        // Pair gate (2026-10-06): freeze's config loader ALWAYS answers
        // agent/command/mode ({} when unset) + username (system user) —
        // keypaths seen live under an identical minimal fixture; a raw echo
        // of a config missing those keys diverged. insert-if-absent: real
        // configs already carry agent/command and are untouched (keys_subset
        // recorded ⊆ got also gains username, which real freeze always had).
        let mut config = raw_config.clone(); // raw file until M4 plugins add agents (keys_subset)
        if let Some(obj) = config.as_object_mut() {
            for k in ["agent", "command", "mode"] {
                obj.entry(k).or_insert(json!({}));
            }
            if !obj.contains_key("username")
                && let Ok(u) = std::env::var("USER").or_else(|_| std::env::var("LOGNAME"))
                && !u.is_empty()
            {
                obj.insert("username".into(), json!(u));
            }
        }
        let (agent, api_agent) = build_agents(&raw_config);
        let command = build_commands(&raw_config)?;
        let (config_providers, provider) = build_providers(&raw_config, &auth, &cache);

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
    let extra_perm = v.get("permission").cloned();
    let obj = item.as_object_mut().expect("agent object");
    for (k, val) in v {
        if k == "disable" || k == "permission" {
            continue;
        }
        if k == "top_p" {
            obj.insert("topP".into(), val.clone());
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
        rules.extend(perm_from_config(&perm));
        obj.insert("permission".into(), Value::Array(rules));
    }
}

fn build_commands(cfg: &Value) -> Result<Vec<Value>> {
    let init = include_str!("../assets/initialize.txt");
    let review = include_str!("../assets/review.txt");
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
    ];
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
        let cfg = read_json(&format!("{home}/.config/opencode/opencode.json"))
            .unwrap_or_else(|| json!({}));
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
                    let key = auth
                        .pointer(&format!("/{pid}/key"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    endpoints.insert(pid.clone(), (base.to_string(), key.to_string()));
                }
            }
        }

        // B2 (K-MODEL-STATE): opencode public-tier endpoint (pure fn below).
        if let Some(pair) = opencode_public_endpoint(&cfg, &auth, cache) {
            endpoints.insert("opencode".into(), pair);
        }

        // pricing from models cache: (in, out, cache_read) USD per MTok
        let mut pricing = std::collections::HashMap::new();
        let mut limits = std::collections::HashMap::new();
        if let Some(provs) = cache.as_object() {
            for (pid, p) in provs {
                if let Some(models) = p.get("models").and_then(|v| v.as_object()) {
                    for (mid, m) in models {
                        limits.entry((pid.clone(), mid.clone())).or_insert_with(|| {
                            m.get("limit").cloned().unwrap_or(serde_json::Value::Null)
                        });
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
            default_model,
            systems,
            default_agent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let paths = watch_paths(dp);
        assert_eq!(
            paths.len(),
            6,
            "opencode.json, overlay, auth, auth-overlay, models cache, state model"
        );
        assert!(
            paths[0].ends_with(".config/opencode/opencode.json"),
            "shared config first: {:?}",
            paths[0]
        );
        assert_eq!(paths[3], dp.join("auth-overlay.json"), "data-dir overlay");
        assert!(
            paths[5].ends_with(".local/state/opencode/model.json"),
            "state model (default-model hot-swap): {:?}",
            paths[5]
        );
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
