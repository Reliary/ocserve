//! v2 `/api/*` surface (opencode 1.18.31 contract).
//!
//! The 1.18.31 TUI and web UI are hybrid clients: their `SyncProvider` boots
//! on v1 paths (served elsewhere in this crate) while `DataProvider` and the
//! session view call the v2 SDK (`client.v2.*`), which resolves to `/api/*`.
//! Before this module every `/api/*` call fell through the web-UI catch-all
//! to SPA **HTML**, and JSON clients that parse it degrade or crash (e.g. the
//! `session.diff` → `SidebarFiles.flatMap` TypeError on an HTML string,
//! 2026-10-08).
//!
//! Shapes are taken from the vendored OpenAPI contract
//! (`bench/openapi/1.18.31.json`, served at `/doc`) and probed live from the
//! freeze. Envelope rules captured live:
//!   - `{location, data}` for location-scoped collections (agent/model/…),
//!   - `{data: …}` for single resources,
//!   - `{data, cursor:{previous,next}}` for paginated message lists.
//!
//! The v2 session prompt route does NOT duplicate the prompt loop — it
//! normalizes the v2 body and calls the same `ocserve_core::prompt::run_prompt`
//! executor the v1 routes use (single owner of the state machine).

use crate::{ApiError, AppState, HttpError, api_location, run_blocking};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::Arc;

/// GET /api/health — `{healthy:true}` (schema: enum [true]).
pub async fn health() -> impl IntoResponse {
    Json(json!({"healthy": true}))
}

/// GET /api/location — bare LocationInfo (not enveloped).
pub async fn location(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(api_location(&st))
}

// ---------------------------------------------------------------------------
// providers / models
// ---------------------------------------------------------------------------

/// Provider ids considered "available" on the v2 wire, mirroring upstream's
/// `CatalogV2.provider.available`:
///   available = !disabled && (apiKey is a string || integration connected ||
///                              no integration at all)
/// Translated to ocserve's data: a provider is available when it is the
/// built-in `opencode` (keyless `public`), has an explicit non-empty
/// `options.apiKey` in config, or is a custom provider absent from the models
/// catalog (hence with no integration record). This reproduces the observed
/// freeze set `{opencode, entrim, nube}` from this box's config.
fn available_provider_ids(st: &AppState, catalog: &Value) -> Vec<String> {
    let payloads = st.payloads.read();
    let connected: Vec<String> = payloads
        .provider
        .get("connected")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let cfg_providers = payloads.config.get("provider").and_then(|v| v.as_object());
    let cat_obj = catalog.as_object();
    connected
        .into_iter()
        .filter(|pid| {
            if pid == "opencode" {
                return true;
            }
            let has_cfg_key = cfg_providers
                .and_then(|c| c.get(pid))
                .and_then(|v| v.pointer("/options/apiKey"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty());
            if has_cfg_key {
                return true;
            }
            // not in the models catalog => no integration record => available
            cat_obj.map(|c| !c.contains_key(pid)).unwrap_or(true)
        })
        .collect()
}

/// Project a v1 model entry to the v2 `ModelV2Info` shape (freeze-probed).
fn project_model_v2(v1: &Value) -> Value {
    let id = v1.get("id").cloned().unwrap_or(Value::Null);
    let provider_id = v1.get("providerID").cloned().unwrap_or(Value::Null);
    let api = v1.get("api").cloned().unwrap_or(json!({}));
    // v2 `ModelApi` is a discriminated union (spec 1.18.31):
    //   {id, type:"aisdk", package, url[, settings]}  — npm-backed
    //   {id, type:"native", url, settings}            — built-in
    // The v1 entry carries `npm` when npm-backed; presence picks the arm.
    let npm = api
        .get("npm")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let mut v2api = match npm {
        Some(pkg) => json!({"id": id, "type": "aisdk", "package": pkg}),
        None => json!({"id": id, "type": "native", "settings": {}}),
    };
    if let Some(url) = api.get("url") {
        v2api["url"] = url.clone();
    }
    // capabilities boolean map → v2 {tools, input[], output[]}
    let caps = v1.get("capabilities").cloned().unwrap_or(json!({}));
    let arr_of = |keys: &[&str]| -> Vec<Value> {
        keys.iter()
            .filter(|k| caps.get(**k).and_then(|v| v.as_bool()).unwrap_or(false))
            .map(|k| json!(k))
            .collect()
    };
    let input = {
        let mut v = vec![json!("text")];
        if caps
            .get("attachment")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            v.push(json!("image"));
        }
        v
    };
    let _ = arr_of;
    let capabilities = json!({
        "tools": caps.get("toolcall").and_then(|v| v.as_bool()).unwrap_or(false),
        "input": input,
        "output": [json!("text")],
    });
    // cost (single-element array form in v2)
    let cost = v1.get("cost").cloned().unwrap_or(json!({}));
    let cache = cost.get("cache").cloned().unwrap_or(json!({}));
    let cost_v2 = json!([{
        "input": cost.get("input").cloned().unwrap_or(json!(0)),
        "output": cost.get("output").cloned().unwrap_or(json!(0)),
        "cache": {
            "read": cache.get("read").cloned().unwrap_or(json!(0)),
            "write": cache.get("write").cloned().unwrap_or(json!(0)),
        }
    }]);
    let limit = v1.get("limit").cloned().unwrap_or(json!({}));
    // variants: v2 `ModelV2Info.variants` is an array of
    // `{id, headers, body}` objects (spec 1.18.31). The v1 entry keys
    // variants by name; each becomes one object carrying its own body when
    // the v1 variant has one.
    let variants: Vec<Value> = v1
        .get("variants")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, vv)| {
                    let body = vv
                        .get("body")
                        .or_else(|| vv.get("settings"))
                        .cloned()
                        .unwrap_or(json!({}));
                    json!({"id": k, "headers": {}, "body": body})
                })
                .collect()
        })
        .unwrap_or_default();
    let released = v1
        .get("release_date")
        .and_then(|v| v.as_str())
        .and_then(parse_release_ms);
    let mut out = json!({
        "id": id,
        "providerID": provider_id,
        "name": v1.get("name").cloned().unwrap_or(Value::Null),
        "api": v2api,
        "capabilities": capabilities,
        "request": {"headers": {}, "body": {}},
        "variants": variants,
        "time": {"released": released.unwrap_or(0)},
        "cost": cost_v2,
        "status": v1.get("status").cloned().unwrap_or(json!("active")),
        "enabled": v1.get("status").and_then(|s| s.as_str()).map(|s| s != "deprecated").unwrap_or(true),
        "limit": {"context": limit.get("context").cloned().unwrap_or(json!(0)), "output": limit.get("output").cloned().unwrap_or(json!(0))},
    });
    if let Some(fam) = v1.get("family") {
        out["family"] = fam.clone();
    }
    out
}

fn parse_release_ms(date: &str) -> Option<i64> {
    // "YYYY-MM-DD" → epoch ms at UTC midnight (upstream `time.released`).
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    // days since epoch (civil-from-days, Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400_000)
}

/// GET /api/provider — `{location, data:[ProviderV2Info]}` for available providers.
pub async fn list_providers(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let avail = {
        let catalog = read_catalog_value();
        available_provider_ids(&st, &catalog)
    };
    let payloads = st.payloads.read();
    let all = payloads
        .provider
        .get("all")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let data: Vec<Value> = all
        .iter()
        .filter(|p| {
            p.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| avail.iter().any(|a| a == id))
        })
        .map(project_provider_v2)
        .collect();
    Json(json!({"location": api_location(&st), "data": data}))
}

fn project_provider_v2(v1: &Value) -> Value {
    let id = v1.get("id").cloned().unwrap_or(Value::Null);
    let npm = v1
        .pointer("/models")
        .and_then(|m| m.as_object())
        .and_then(|m| m.values().next())
        .and_then(|m| m.pointer("/api/npm"))
        .and_then(|v| v.as_str());
    let url = v1
        .pointer("/options/baseURL")
        .and_then(|v| v.as_str())
        .or_else(|| {
            v1.pointer("/models")
                .and_then(|m| m.as_object())
                .and_then(|m| m.values().next())
                .and_then(|m| m.pointer("/api/url"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");
    let mut api = json!({"type": if npm.is_some() { "aisdk" } else { "native" }});
    if let Some(n) = npm {
        api["package"] = json!(n);
    }
    if !url.is_empty() {
        api["url"] = json!(url);
    }
    api["settings"] = json!({});
    // request.body: carry apiKey only when it is the keyless public tier or a
    // config-supplied key (never a secret read from auth.json — v2 freeze
    // does not expose those either).
    let mut body = json!({});
    if id == json!("opencode") {
        body = json!({"apiKey": "public"});
    } else if let Some(k) = v1.pointer("/options/apiKey").and_then(|v| v.as_str())
        && !k.is_empty()
    {
        body = json!({"apiKey": k});
    }
    json!({
        "id": id,
        "name": v1.get("name").cloned().unwrap_or(Value::Null),
        "api": api,
        "request": {"headers": {}, "body": body},
    })
}

/// GET /api/provider/{providerID} — `{location, data:ProviderV2Info}`.
pub async fn get_provider(
    State(st): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let payloads = st.payloads.read();
    let all = payloads
        .provider
        .get("all")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    match all
        .iter()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(&provider_id))
    {
        Some(p) => Ok(Json(
            json!({"location": api_location(&st), "data": project_provider_v2(p)}),
        )),
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            name: "NotFoundError",
            message: format!("Provider not found: {provider_id}"),
        }),
    }
}

/// GET /api/model — `{location, data:[ModelV2Info]}`, available providers only,
/// non-deprecated models, sorted by release descending (upstream order).
pub async fn list_models(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let avail = {
        let catalog = read_catalog_value();
        available_provider_ids(&st, &catalog)
    };
    let payloads = st.payloads.read();
    let all = payloads
        .provider
        .get("all")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut data: Vec<Value> = Vec::new();
    for p in &all {
        let pid = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !avail.iter().any(|a| a == pid) {
            continue;
        }
        if let Some(models) = p.get("models").and_then(|m| m.as_object()) {
            for m in models.values() {
                if m.get("status").and_then(|s| s.as_str()) == Some("deprecated") {
                    continue;
                }
                data.push(project_model_v2(m));
            }
        }
    }
    data.sort_by(|a, b| {
        let ra = a
            .pointer("/time/released")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let rb = b
            .pointer("/time/released")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        rb.cmp(&ra)
    });
    Json(json!({"location": api_location(&st), "data": data}))
}

/// The models catalog (models.json) as a Value, best-effort (empty on miss).
fn read_catalog_value() -> Value {
    // Reuse the same discovery rule as the CLI runtime: cache models.json.
    let home = std::env::var("HOME").unwrap_or_default();
    let path = std::path::Path::new(&home).join(".cache/opencode/models.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

// ---------------------------------------------------------------------------
// skills / integrations / references
// ---------------------------------------------------------------------------

/// GET /api/skill — `{location, data:[SkillV2Info]}`. ocserve exposes the one
/// built-in skill it ships (the `customize-opencode` command, source "skill").
pub async fn list_skills(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let payloads = st.payloads.read();
    let data: Vec<Value> = payloads
        .command
        .iter()
        .filter(|c| c.get("source").and_then(|v| v.as_str()) == Some("skill"))
        .map(|c| {
            json!({
                "name": c.get("name").cloned().unwrap_or(Value::Null),
                "description": c.get("description").cloned().unwrap_or(json!("")),
                "slash": false,
                "location": "",
                "content": c.get("template").cloned().unwrap_or(json!("")),
            })
        })
        .collect();
    Json(json!({"location": api_location(&st), "data": data}))
}

/// Integration methods for one catalog provider (key + env), freeze-probed.
fn integration_entry(id: &str, name: &str, env: &[Value]) -> Value {
    let mut methods = vec![json!({"type": "key"})];
    if !env.is_empty() {
        methods.push(json!({"type": "env", "names": env}));
    }
    json!({"id": id, "name": name, "methods": methods, "connections": []})
}

/// Build the integration list from the models catalog (+ the catalog-less
/// custom providers seen in config). The catalog is the source of the 226
/// entries freeze serves; `connections` is always empty on this box (no
/// OAuth/key integrations connected through the v2 store).
fn all_integrations(st: &AppState) -> Vec<Value> {
    let catalog = read_catalog_value();
    let mut out: Vec<Value> = Vec::new();
    if let Some(map) = catalog.as_object() {
        for (id, entry) in map {
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or(id);
            let env = entry
                .get("env")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            out.push(integration_entry(id, name, &env));
        }
    }
    // catalog-less custom providers (e.g. entrim/nube here) are integrations too
    let payloads = st.payloads.read();
    if let Some(cps) = payloads.config.get("provider").and_then(|v| v.as_object()) {
        for (id, v) in cps {
            if catalog
                .as_object()
                .map(|c| c.contains_key(id))
                .unwrap_or(false)
            {
                continue;
            }
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or(id);
            out.push(integration_entry(id, name, &[]));
        }
    }
    out
}

/// GET /api/integration — `{location, data:[IntegrationInfo]}`.
pub async fn list_integrations(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let data = all_integrations(&st);
    Json(json!({"location": api_location(&st), "data": data}))
}

/// GET /api/integration/{integrationID} — single integration.
pub async fn get_integration(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let items = all_integrations(&st);
    match items
        .into_iter()
        .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(&id))
    {
        Some(i) => Ok(Json(json!({"location": api_location(&st), "data": i}))),
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            name: "NotFoundError",
            message: format!("Integration not found: {id}"),
        }),
    }
}

/// GET /api/reference — `{location, data:[]}` (no references configured).
pub async fn list_references(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({"location": api_location(&st), "data": []}))
}

// ---------------------------------------------------------------------------
// filesystem
// ---------------------------------------------------------------------------

/// GET /api/fs/find?query=&type=&limit= — `{location, data:[FileSystemEntry]}`.
pub async fn fs_find(
    State(st): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/")).to_path_buf();
    let query = q
        .get("query")
        .map(String::as_str)
        .unwrap_or("")
        .to_lowercase();
    if query.is_empty() {
        return Ok(Json(json!({"location": api_location(&st), "data": []})));
    }
    let want_type = q.get("type").map(String::as_str).unwrap_or("").to_string();
    let limit: usize = q
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let glob_mode = query.contains('*') || query.contains('?');
    let walk_base = base.clone();
    let out = tokio::task::spawn_blocking(move || {
        crate::walk_files(&walk_base, &query, &want_type, limit, glob_mode)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalServerError",
        message: format!("fs_find worker: {e}"),
    })?
    .map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: format!("fs_find: {e:#}"),
    })?;
    let data: Vec<Value> = out
        .into_iter()
        .map(|p| {
            let is_dir = p.ends_with('/')
                || std::fs::metadata(base_join(&base, &p))
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
            json!({"path": p, "type": if is_dir { "directory" } else { "file" }})
        })
        .collect();
    Ok(Json(json!({"location": api_location(&st), "data": data})))
}

fn base_join(base: &std::path::Path, rel: &str) -> std::path::PathBuf {
    base.join(rel.trim_end_matches('/'))
}

/// GET /api/fs/list?path= — direct children of `path` (default ".").
pub async fn fs_list(
    State(st): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/")).to_path_buf();
    let rel = q.get("path").cloned().unwrap_or_default();
    // Containment (the 2026-10-11 traversal fix): resolve + canonicalize and
    // reject a path that escapes the project root. `git rev-parse`-style
    // lexical `..` never listed /etc on freeze (it 500s); ocserve listed it.
    let dir = resolve_within(&base, &rel).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: "path escapes the project directory".into(),
    })?;
    let entries = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        let rd = std::fs::read_dir(&dir)?;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(json!({
                "path": if is_dir { format!("{name}/") } else { name },
                "type": if is_dir { "directory" } else { "file" },
            }));
        }
        out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(out)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalServerError",
        message: format!("fs_list worker: {e}"),
    })?
    .map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: format!("fs_list: {e:#}"),
    })?;
    Ok(Json(
        json!({"location": api_location(&st), "data": entries}),
    ))
}

/// Resolve `rel` under `base` and refuse to escape it. Canonicalizes the base
/// (the project root exists) and the joined target; a target that does not
/// canonicalize (missing) is still allowed only if its lexical form stays
/// inside `base` (so `fs/list?path=missing` reports "No such file" rather than
/// silently passing). `..` that resolves outside `base` is rejected.
fn resolve_within(base: &std::path::Path, rel: &str) -> std::io::Result<std::path::PathBuf> {
    let base_canon = base.canonicalize()?;
    let joined = base_canon.join(rel.trim_start_matches('/'));
    match joined.canonicalize() {
        Ok(real) => {
            if real.starts_with(&base_canon) {
                Ok(real)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "path escapes the project directory",
                ))
            }
        }
        // not found: fall back to a lexical containment check on the joined path
        Err(_) => {
            let lex = lexically_normalize(&joined);
            if lex.starts_with(&base_canon) {
                Ok(joined)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "path escapes the project directory",
                ))
            }
        }
    }
}

/// Lexically resolve `.`/`..` without touching the filesystem (used only for
/// the not-found branch; a normalized join stays comparable to a canonical base).
fn lexically_normalize(p: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// GET /api/fs/read/*path — raw bytes of one file (Uint8Array on the wire).
pub async fn fs_read(
    State(st): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/")).to_path_buf();
    let full = base.join(path.trim_start_matches('/'));
    let canon = full.canonicalize().map_err(|_| ApiError {
        status: StatusCode::NOT_FOUND,
        name: "NotFoundError",
        message: format!("File not found: {path}"),
    })?;
    if !canon.starts_with(&base) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "path escapes the project directory".into(),
        });
    }
    let bytes = tokio::fs::read(&canon).await.map_err(|e| ApiError {
        status: StatusCode::NOT_FOUND,
        name: "NotFoundError",
        message: format!("File not found: {path} ({e})"),
    })?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// session diff — the proven TUI sidebar crash path
// ---------------------------------------------------------------------------

/// GET /session/{id}/diff — revert-range file diffs. ocserve stores the revert
/// marker but has no snapshot engine (D-REVERT-NOSNAP), so the diff is `[]`
/// unless a marker carries a `files` array (then echoed). Crucially this is
/// **JSON**, not SPA HTML: the TUI's `SidebarFiles` flatMaps the result and an
/// HTML string crashes it (2026-10-08).
pub async fn session_diff(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    let s = run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(internal)?;
    let files = s
        .as_ref()
        .and_then(|s| s.get("revert"))
        .and_then(|r| r.get("files"))
        .cloned()
        .unwrap_or(json!([]));
    Ok(Json(files))
}

fn internal(e: anyhow::Error) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    }
}

/// v2 tagged SessionNotFoundError (`{_tag, sessionID, message}`) — freeze
/// returns this for `/api/session/{id}/...` on an unknown session; ocserve
/// previously 204'd (blind UPDATE) or used the v1 `{name,data}` envelope.
fn session_not_found(id: &str) -> crate::HttpError {
    crate::HttpError::TaggedData {
        status: StatusCode::NOT_FOUND,
        tag: "SessionNotFoundError",
        fields: json!({"sessionID": id, "message": format!("Session not found: {id}")}),
    }
}

// ---------------------------------------------------------------------------
// session core
// ---------------------------------------------------------------------------

/// Project a stored v1 session row to `SessionV2Info` (freeze-probed).
fn session_v2(s: &Value) -> Value {
    let dir = s.get("directory").and_then(|v| v.as_str()).unwrap_or("/");
    let mut out = json!({
        "id": s.get("id").cloned().unwrap_or(Value::Null),
        "projectID": s.get("projectID").cloned().unwrap_or(json!("global")),
        "cost": s.get("cost").cloned().unwrap_or(json!(0)),
        "tokens": s.get("tokens").cloned().unwrap_or(json!({"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}})),
        "time": s.get("time").cloned().unwrap_or(json!({"created":0,"updated":0})),
        "title": s.get("title").cloned().unwrap_or(json!("")),
        "location": {"directory": dir},
    });
    if let Some(pid) = s.get("parentID").and_then(|v| v.as_str()) {
        out["parentID"] = json!(pid);
    }
    if let Some(a) = s.get("agent").and_then(|v| v.as_str()) {
        out["agent"] = json!(a);
    }
    if let Some(m) = s.get("model")
        && !m.is_null()
    {
        out["model"] = m.clone();
    }
    // subpath: path relative to the first path segment (freeze strips the
    // leading "/", e.g. directory "/srv/project" -> "srv/project").
    if let Some(path) = s.get("path").and_then(|v| v.as_str())
        && !path.is_empty()
    {
        out["subpath"] = json!(path);
    }
    // revert: SessionV2Info carries RevertState (messageID-bearing object).
    if let Some(r) = s.get("revert")
        && !r.is_null()
    {
        out["revert"] = r.clone();
    }
    out
}

/// GET /api/session — `{data:[SessionV2Info], cursor:{previous,next}}`.
pub async fn list_sessions(
    State(st): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, crate::HttpError> {
    // Query decode precedes the handler (freeze): invalid limit/order/cursor
    // → 400 with the v2 tagged envelope
    // `{"_tag":"InvalidRequestError","message":…,"kind":"Query"}`.
    let q_err = |message: String| crate::HttpError::TaggedData {
        status: StatusCode::BAD_REQUEST,
        tag: "InvalidRequestError",
        fields: json!({"message": message, "kind": "Query"}),
    };
    let limit: usize = match q.get("limit") {
        None => 100,
        Some(raw) => {
            let n: f64 = raw
                .parse()
                .map_err(|_| q_err("Expected an integer, got NaN\n  at [\"limit\"]".into()))?;
            if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
                return Err(q_err(format!(
                    "Expected an integer, got {raw}\n  at [\"limit\"]"
                )));
            }
            (n as usize).clamp(1, 200)
        }
    };
    if let Some(order) = q.get("order")
        && order != "asc"
        && order != "desc"
    {
        return Err(q_err(format!(
            "Expected \"asc\" | \"desc\", got \"{order}\"\n  at [\"order\"]"
        )));
    }
    if let Some(cur) = q.get("cursor")
        && ocserve_store::decode_cursor(cur).is_err()
    {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidCursorError",
            fields: json!({"message": "Invalid cursor"}),
        });
    }
    let roots = q.get("roots").map(|v| v == "true").unwrap_or(false);
    let search = q.get("search").cloned().unwrap_or_default().to_lowercase();
    let sid = q.get("sessionID").cloned();
    let db = st.db.clone();
    let mut sessions = run_blocking(&db, ocserve_store::load_sessions_wire)
        .await
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
    if roots {
        sessions.retain(|s| s.get("parentID").map(|p| p.is_null()).unwrap_or(true));
    }
    if !search.is_empty() {
        sessions.retain(|s| {
            s.get("title")
                .and_then(|v| v.as_str())
                .map(|t| t.to_lowercase().contains(&search))
                .unwrap_or(false)
        });
    }
    let _ = sid;
    let total = sessions.len();
    let more = total > limit;
    let window: Vec<Value> = sessions
        .into_iter()
        .take(limit)
        .map(|s| session_v2(&s))
        .collect();
    // freeze always emits both cursor keys (opaque base64url cursors); ocserve
    // does not paginate v2 session list, so both are null (shape-faithful).
    let _ = more;
    Ok(Json(
        json!({"data": window, "cursor": {"previous": Value::Null, "next": Value::Null}}),
    ))
}

/// GET /api/session/active — `{data:{}}` (no per-request foreground drains
/// are tracked on the v2 wire; ocserve's own prompt locks are v1 status).
pub async fn active_sessions() -> impl IntoResponse {
    Json(json!({"data": {}}))
}

/// GET /api/session/{sessionID} — `{data:SessionV2Info}`.
pub async fn get_session(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, crate::HttpError> {
    let sid = id.clone();
    let s = run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
    match s {
        Some(s) => Ok(Json(json!({"data": session_v2(&s)}))),
        // freeze v2 uses the tagged SessionNotFoundError (probe), not the v1
        // {name,data} envelope.
        None => Err(session_not_found(&id)),
    }
}

/// GET /api/session/{sessionID}/message — paginated projected messages.
pub async fn list_messages(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, crate::HttpError> {
    let limit: usize = q
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let s = {
        let sid = id.clone();
        run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
            .await
            .map_err(|e| crate::HttpError::Api(internal(e)))?
    };
    if s.is_none() {
        return Err(session_not_found(&id));
    }
    let sid = id.clone();
    let msgs = run_blocking(&st.db, move |db| {
        ocserve_store::page_messages(db, &sid, limit as u64, None)
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let data: Vec<Value> = msgs
        .0
        .into_iter()
        .filter_map(|(mid, info)| project_message_v2(&mid, &info))
        .collect();
    // freeze always emits BOTH cursor keys (previous/next), null when absent.
    let mut cursor = json!({"previous": Value::Null, "next": Value::Null});
    if let Some(next) = msgs.2 {
        cursor["next"] = json!(next);
    }
    Ok(Json(json!({"data": data, "cursor": cursor})))
}

/// Project a stored v1 message `info` (JSON text) to a v2 `SessionMessage`.
/// The v2 message model is a projection of the same durable stream; we map
/// the role/type to the closest v2 variant and always return JSON.
fn project_message_v2(id: &str, info: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(info).ok()?;
    let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let created = v.pointer("/time/created").cloned().unwrap_or(json!(0));
    let time = json!({"created": created});
    match role {
        "user" => {
            let text = v
                .pointer("/summary/title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Some(json!({"id": id, "time": time, "type": "user", "text": text}))
        }
        "assistant" => Some(json!({
            "id": id,
            "time": time,
            "type": "assistant",
            "agent": v.get("agent").cloned().unwrap_or(json!("build")),
            "model": {"id": v.get("modelID").cloned().unwrap_or(json!("")), "providerID": v.get("providerID").cloned().unwrap_or(json!(""))},
            "content": [],
            "cost": v.get("cost").cloned().unwrap_or(json!(0)),
            "tokens": normalize_tokens(v.get("tokens")),
        })),
        _ => None,
    }
}

fn normalize_tokens(t: Option<&Value>) -> Value {
    let t = t.cloned().unwrap_or(json!({}));
    let cache = t.get("cache").cloned().unwrap_or(json!({}));
    json!({
        "input": t.get("input").cloned().unwrap_or(json!(0)),
        "output": t.get("output").cloned().unwrap_or(json!(0)),
        "reasoning": t.get("reasoning").cloned().unwrap_or(json!(0)),
        "cache": {"read": cache.get("read").cloned().unwrap_or(json!(0)), "write": cache.get("write").cloned().unwrap_or(json!(0))},
    })
}

/// GET /api/session/{sessionID}/message/{messageID} — single message.
pub async fn get_message(
    State(st): State<Arc<AppState>>,
    Path((id, mid)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    let mid2 = mid.clone();
    let row = run_blocking(&st.db, move |db| {
        ocserve_store::message_by_id(db, &sid, &mid2)
    })
    .await
    .map_err(internal)?;
    let info = row
        .and_then(|v| v.get("info").map(|i| i.to_string()))
        .and_then(|info| project_message_v2(&mid, &info));
    match info {
        Some(m) => Ok(Json(json!({"data": m}))),
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            name: "NotFoundError",
            message: format!("Message not found: {mid} (session {id})"),
        }),
    }
}

/// GET /api/session/{sessionID}/context — `{data:[SessionMessage.Message]}`.
/// Projection over the stored message stream (thinner than upstream's full
/// event-sourcing; documented divergence).
pub async fn session_context(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, crate::HttpError> {
    list_messages(State(st), Path(id), Query(std::collections::HashMap::new())).await
}

/// GET /api/session/{sessionID}/history — `{data:[DurableEvent], hasMore}`.
/// ocserve serves its own event log; the wire shape matches, semantics are
/// a subset (documented).
pub async fn session_history(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    let events = run_blocking(&st.db, move |db| {
        ocserve_store::load_session_events(db, &sid, 100)
    })
    .await
    .map_err(internal)?;
    Ok(Json(json!({"data": events, "hasMore": false})))
}

// ---------------------------------------------------------------------------
// permissions / questions
// ---------------------------------------------------------------------------

/// GET /api/permission/saved — `{data:[PermissionSavedInfo]}`.
pub async fn permission_saved(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    // ocserve stores per-session always-grants as string keys, not the v2
    // PermissionSavedInfo rows; there is no project-level saved set => [].
    let _ = &st;
    Json(json!({"data": []}))
}

/// GET /api/permission/request — `{location, data:[...]}` pending asks.
pub async fn permission_request(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let data = st.gate.list();
    Json(json!({"location": api_location(&st), "data": data}))
}

/// GET /api/question/request — `{location, data:[...]}` pending questions.
pub async fn question_request(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let data = st.question_gate.list();
    Json(json!({"location": api_location(&st), "data": data}))
}

/// GET /api/session/{sessionID}/permission — `{data:[]}` (per-session asks).
pub async fn session_permission(
    State(st): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> impl IntoResponse {
    Json(json!({"data": st.gate.list()}))
}

/// GET /api/session/{sessionID}/question — `{data:[]}` (per-session questions).
pub async fn session_question(
    State(st): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> impl IntoResponse {
    Json(json!({"data": st.question_gate.list()}))
}

// ---------------------------------------------------------------------------
// writes: session lifecycle (shared executor for prompt)
// ---------------------------------------------------------------------------

/// POST /api/session/{sessionID}/agent — switch agent (NoContent).
pub async fn switch_agent(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, crate::HttpError> {
    let agent = body.get("agent").and_then(|v| v.as_str()).unwrap_or("");
    set_session_field(&st, &id, "agent", json!(agent))?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/model — switch model (NoContent).
pub async fn switch_model(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, crate::HttpError> {
    set_session_field(
        &st,
        &id,
        "model",
        body.get("model").cloned().unwrap_or(json!({})),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

fn set_session_field(
    st: &AppState,
    id: &str,
    field: &str,
    val: Value,
) -> Result<(), crate::HttpError> {
    let col = match field {
        "agent" => "agent",
        "model" => "model",
        _ => {
            return Err(crate::HttpError::Api(ApiError {
                status: StatusCode::BAD_REQUEST,
                name: "BadRequest",
                message: format!("unknown field {field}"),
            }));
        }
    };
    // freeze 404s an unknown session (SessionNotFoundError); the old code ran
    // the UPDATE blind and 204'd regardless of rows affected.
    if !ocserve_store::session_exists(&st.db, id).map_err(|e| crate::HttpError::Api(internal(e)))? {
        return Err(session_not_found(id));
    }
    let text = if field == "model" && val.is_object() {
        val.to_string()
    } else if let Some(s) = val.as_str() {
        s.to_string()
    } else {
        val.to_string()
    };
    st.writer
        .write(vec![ocserve_store::WriteOp::Sql {
            sql: format!("UPDATE session SET {col} = ?2 WHERE id = ?1"),
            params: vec![id.into(), text.into()],
        }])
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// experimental stubs (freeze-probed shapes)
// ---------------------------------------------------------------------------

/// GET /experimental/console/orgs — `{orgs:[]}`.
pub async fn console_orgs() -> impl IntoResponse {
    Json(json!({"orgs": []}))
}

/// GET /experimental/workspace — `[]`.
pub async fn workspace_list() -> impl IntoResponse {
    Json(json!([]))
}

/// GET /experimental/workspace/status — `[]`.
pub async fn workspace_status() -> impl IntoResponse {
    Json(json!([]))
}

/// GET /experimental/workspace/adapter — `[{type,name,description}]`.
pub async fn workspace_adapter() -> impl IntoResponse {
    Json(json!([{"type": "worktree", "name": "Worktree", "description": "Create a git worktree"}]))
}

/// GET /experimental/worktree — `[]`.
pub async fn worktree_list() -> impl IntoResponse {
    Json(json!([]))
}

// ---------------------------------------------------------------------------
// session writes — share the v1 prompt executor (no loop duplication)
// ---------------------------------------------------------------------------

/// POST /api/session/{sessionID}/prompt — spec `SessionInput.Admitted`.
/// Normalizes the v2 body `{id?, prompt:{text,files,agents}, delivery, resume}`
/// into the v1 payload form and runs the SAME `ocserve_core::prompt::run_prompt`
/// the v1 `/session/{id}/message` and `/prompt_async` routes use. This route is
/// synchronous (the v1 `session.prompt` semantics), so the loop stays in one
/// place; only the body shape differs.
pub async fn prompt(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    // boundary decode FIRST with the v2 envelope (`_tag:InvalidRequestError`,
    // field-probes.md) — session/endpoint checks follow.
    let input = crate::decode_v2(ocserve_core::wire::V2PromptInput::decode(&body))?;
    let req = ocserve_core::wire::PromptRequest {
        message_id: input.id.clone(), // already validated `^msg_`
        parts: normalize_v2_prompt_parts(&input),
        ..Default::default()
    };
    let ctx = crate::build_prompt_context(&st, &req, &id)?;
    let release = crate::lock_session(&st, &id).await?;
    let Ok(_guard) = release.arc().try_lock() else {
        return Err(crate::session_busy(&id).into());
    };
    let writer = st.writer.clone();
    let (info, _parts) = ocserve_core::prompt::run_prompt(&ctx, &writer, &id, &req)
        .await
        .map_err(crate::prompt_err)?;
    drop(_guard);
    let msg_id = info.get("id").cloned().unwrap_or(Value::Null);
    Ok(Json(json!({
        "data": {
            "id": msg_id,
            "sessionID": id,
            "admittedSeq": 0,
            "prompt": body.get("prompt").cloned().unwrap_or(json!({"text": ""})),
            // absent/null delivery defaults to "steer" (probe: fresh session,
            // no delivery field → "delivery":"steer"; NOT "queue").
            "delivery": Value::String(input.delivery.unwrap_or_else(|| "steer".into())),
            "timeCreated": info.pointer("/time/created").cloned().unwrap_or(json!(0)),
        }
    })))
}

/// v2 `PromptInput` → v1 parts array (text + file + agent attachments),
/// built from the decoded struct (wire names live in wire.rs).
fn normalize_v2_prompt_parts(input: &ocserve_core::wire::V2PromptInput) -> Vec<Value> {
    let mut parts = Vec::new();
    // text is required by decode — always present (empty string included,
    // matching the old `if let Some(text)` behavior for `text:""`).
    parts.push(json!({"type": "text", "text": input.prompt_text}));
    for f in &input.prompt_files {
        parts.push(json!({"type": "file", "file": f}));
    }
    for a in &input.prompt_agents {
        parts.push(json!({"type": "agent", "agent": a}));
    }
    parts
}

/// POST /api/session/{sessionID}/interrupt — NoContent (abort the task).
pub async fn interrupt(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if let Some((_, handle)) = st.prompt_tasks.lock().remove(&id) {
        handle.abort();
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/compact — NoContent (triggers summarize).
pub async fn compact(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Freeze: ServiceUnavailable when no model is available; ocserve maps to
    // the same when the session has no resolvable model. Otherwise summarize.
    let payload = ocserve_core::wire::PromptRequest::default();
    let ctx = match crate::build_prompt_context(&st, &payload, &id) {
        Ok(c) => c,
        Err(e) => {
            return Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                name: "ServiceUnavailableError",
                message: e.message,
            });
        }
    };
    let release = crate::lock_session(&st, &id).await?;
    let Ok(_guard) = release.arc().try_lock() else {
        return Err(crate::session_busy(&id));
    };
    let writer = st.writer.clone();
    let _ = crate::run_compact(&st, &ctx, &writer, &id).await;
    drop(_guard);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/wait — freeze returns ServiceUnavailable
/// ("not available yet"); mirror honestly (no long-poll engine).
pub async fn wait(State(_): State<Arc<AppState>>, Path(_id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(
            json!({"_tag": "ServiceUnavailableError", "message": "Session wait is not available yet", "service": "session.wait"}),
        ),
    )
}

/// POST /api/session/{sessionID}/revert/stage — `{data:RevertState}`.
pub async fn revert_stage(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let mid = body.get("messageID").and_then(|v| v.as_str()).unwrap_or("");
    if mid.is_empty() {
        return Err(payload_err("messageID"));
    }
    let mut marker = json!({"messageID": mid, "diff": "", "files": []});
    if let Some(p) = body.get("partID")
        && !p.is_null()
    {
        marker["partID"] = p.clone();
    }
    ocserve_store::set_session_revert(&st.writer, &id, Some(&marker)).map_err(internal)?;
    Ok(Json(json!({"data": marker})))
}

/// POST /api/session/{sessionID}/revert/clear — NoContent (clear marker).
pub async fn revert_clear(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    ocserve_store::set_session_revert(&st.writer, &id, None).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/revert/commit — NoContent (marker stays).
pub async fn revert_commit(
    State(_): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    Ok(StatusCode::NO_CONTENT)
}

/// v2 `/api/*` payload error — probe-pinned envelope (field-probes.md):
/// `{"_tag":"InvalidRequestError","message":"Missing key\n  at [\"<key>\"]",
/// "kind":"Payload"}` [400]. NOTE: differs from v1 routes (name/data nesting)
/// — this is the v2 HttpApi tagged-error shape.
fn payload_err(key: &str) -> HttpError {
    HttpError::TaggedData {
        status: StatusCode::BAD_REQUEST,
        tag: "InvalidRequestError",
        fields: json!({"message": format!("Missing key\n  at [\"{key}\"]"), "kind": "Payload"}),
    }
}

/// POST /api/session/{sessionID}/permission/{requestID}/reply — reply to ask.
/// Body decode precedes the 404 (same Payload envelope as v1); the old
/// `unwrap_or("reject")` silently rejected a real ask on a malformed body.
pub async fn permission_reply(
    State(st): State<Arc<AppState>>,
    Path((_sid, rid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, crate::HttpError> {
    let reply = crate::validate_permission_reply(&body)?;
    if st.gate.reply(&rid, reply) {
        Ok(Json(json!({"data": {}})))
    } else {
        Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "PermissionNotFoundError",
            fields: json!({"requestID": rid, "message": format!("Permission request not found: {rid}")}),
        })
    }
}

/// POST /api/session/{sessionID}/question/{requestID}/reply — reply.
pub async fn question_reply(
    State(st): State<Arc<AppState>>,
    Path((_sid, rid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, crate::HttpError> {
    crate::validate_request_prefix(&rid, "que")?;
    let raw = body
        .get("answers")
        .and_then(|a| a.as_array())
        .ok_or_else(|| {
            crate::HttpError::Api(ApiError {
                status: StatusCode::BAD_REQUEST,
                name: "BadRequest",
                message: "body must be {answers: [[String]]}".into(),
            })
        })?;
    let mut answers: Vec<Vec<String>> = Vec::with_capacity(raw.len());
    for a in raw {
        answers.push(
            a.as_array()
                .map(|r| {
                    r.iter()
                        .map(|l| l.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    if st.question_gate.reply(&rid, answers) {
        Ok(Json(json!({"data": {}})))
    } else {
        Err(crate::question_not_found(&rid))
    }
}

/// POST /api/session/{sessionID}/question/{requestID}/reject — reject.
pub async fn question_reject(
    State(st): State<Arc<AppState>>,
    Path((_sid, rid)): Path<(String, String)>,
) -> Result<Json<Value>, crate::HttpError> {
    crate::validate_request_prefix(&rid, "que")?;
    if st.question_gate.reject(&rid) {
        Ok(Json(json!({"data": {}})))
    } else {
        Err(crate::question_not_found(&rid))
    }
}

/// GET /api/session/{sessionID}/permission/{requestID} — one ask.
pub async fn session_permission_one(
    State(st): State<Arc<AppState>>,
    Path((_sid, rid)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    if !rid.starts_with("per") {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "InvalidRequestError",
            message: format!("Expected a string starting with \"per\", got \"{rid}\""),
        });
    }
    let found = st
        .gate
        .list()
        .into_iter()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(rid.as_str()));
    match found {
        Some(p) => Ok(Json(json!({"data": p}))),
        None => Err(ApiError::not_found(format!(
            "Permission request not found: {rid}"
        ))),
    }
}

/// DELETE /api/permission/saved/{id} — NoContent (best-effort grant removal).
pub async fn permission_saved_delete(
    State(_): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> StatusCode {
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// pty under /api (same implementation as v1 /pty)
// ---------------------------------------------------------------------------

/// GET /api/pty — `{location, data:[Pty]}`.
pub async fn pty_list(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({"location": api_location(&st), "data": st.pty.list_running()}))
}

/// POST /api/pty — create (location-wrapped info).
pub async fn pty_create(
    State(st): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    let input = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    match st.pty.create(&input) {
        Ok(info) => Json(json!({"location": api_location(&st), "data": info})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"name": "UnknownError", "data": {"message": e}})),
        )
            .into_response(),
    }
}

/// GET /api/pty/{ptyID} — get.
pub async fn pty_get(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    match st.pty.get_running(&id).ok() {
        Some(info) => Json(json!({"location": api_location(&st), "data": info})).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"name": "NotFoundError", "data": {"message": format!("pty not found: {id}")}}))).into_response(),
    }
}

/// PUT /api/pty/{ptyID} — update title/size.
pub async fn pty_update(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    let input = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    match st.pty.update(&id, &input) {
        Ok(info) => Json(json!({"location": api_location(&st), "data": info})).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"name": "NotFoundError", "data": {"message": attach_err_msg(&e, &id)}})),
        )
            .into_response(),
    }
}

fn attach_err_msg(e: &ocserve_pty::AttachError, id: &str) -> String {
    match e {
        ocserve_pty::AttachError::NotFound => format!("pty not found: {id}"),
        ocserve_pty::AttachError::Exited => format!("pty exited: {id}"),
    }
}

/// DELETE /api/pty/{ptyID} — remove (true).
pub async fn pty_remove(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match st.pty.remove(&id) {
        Ok(_) => Json(json!({"location": api_location(&st), "data": true})).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"name": "NotFoundError", "data": {"message": attach_err_msg(&e, &id)}})),
        )
            .into_response(),
    }
}

/// POST /api/pty/{ptyID}/connect-token.
pub async fn pty_connect_token(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Freeze shape: `{location, data:{ticket,expires_in}}`. ocserve's ticket
    // is embedded in the same `issue_ticket` payload; surface the `ticket`.
    let t = st.pty.issue_ticket(&id);
    Json(json!({"location": api_location(&st), "data": t})).into_response()
}

// ---------------------------------------------------------------------------
// experimental action stubs (freeze-probed: exact error envelopes)
// ---------------------------------------------------------------------------

/// POST /experimental/console/switch — 400 payload (accountID required).
pub async fn console_switch(Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("accountID").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"accountID\"]", "kind": "Payload"}})),
        )
            .into_response();
    }
    Json(json!({})).into_response()
}

/// POST /experimental/workspace/sync-list — 204.
pub async fn workspace_sync_list() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// POST /experimental/workspace/warp — 400 payload (id required).
pub async fn workspace_warp(Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("id").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"id\"]", "kind": "Payload"}})),
        )
            .into_response();
    }
    Json(json!({})).into_response()
}

/// POST /experimental/worktree — WorktreeNotGitError on non-git.
pub async fn worktree_create(Json(_body): Json<Value>) -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({"name": "WorktreeNotGitError", "data": {"message": "Worktrees are only supported for git projects"}}),
        ),
    )
}

/// POST /experimental/worktree/reset — 400 payload (directory required).
pub async fn worktree_reset(Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("directory").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"directory\"]", "kind": "Payload"}})),
        )
            .into_response();
    }
    Json(json!({})).into_response()
}

/// POST /experimental/control-plane/move-session — 400 (sessionID required).
pub async fn control_move_session(Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("sessionID").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"sessionID\"]", "kind": "Payload"}})),
        )
            .into_response();
    }
    Json(json!({})).into_response()
}

/// POST /experimental/project/{projectID}/copy — 400 (strategy required).
pub async fn project_copy_create(
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if body.get("strategy").is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"_tag": "InvalidRequestError", "message": "Missing key\n  at [\"strategy\"]", "kind": "Payload"})),
        )
            .into_response();
    }
    Json(json!({})).into_response()
}

/// POST /experimental/project/{projectID}/copy/generate-name — random name.
pub async fn project_copy_generate_name(Path(_id): Path<String>) -> impl IntoResponse {
    // Two lowercase words, hyphen-joined (freeze: "glowing-pixel").
    const A: [&str; 8] = [
        "glowing", "silent", "rapid", "amber", "quiet", "brave", "lunar", "vivid",
    ];
    const B: [&str; 8] = [
        "pixel", "river", "comet", "forest", "harbor", "ember", "meadow", "falcon",
    ];
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    Json(json!({"name": format!("{}-{}", A[n % A.len()], B[(n / A.len()) % B.len()])}))
}

/// POST /experimental/project/{projectID}/copy/refresh — 204.
pub async fn project_copy_refresh(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// DELETE /experimental/project/{projectID}/copy — 400 (body required).
pub async fn project_copy_remove(Path(_id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({"_tag": "InvalidRequestError", "message": "Expected object, got undefined", "kind": "Payload"}),
        ),
    )
}

/// POST /experimental/session/{sessionID}/background — false.
pub async fn session_background(Path(_id): Path<String>) -> impl IntoResponse {
    Json(json!(false))
}

/// DELETE /experimental/workspace/{id} — 204.
pub async fn workspace_delete(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// v1 residual: share
// ---------------------------------------------------------------------------

/// POST /session/{id}/share — freeze returns the Session record (share is a
/// local-only marker; ocserve has no remote share backend, so the record is
/// returned with `share` unset — a named divergence `D-SHARE-NOBACKEND`).
pub async fn session_share(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// DELETE /session/{id}/share — freeze returns the (unshared) Session record.
pub async fn session_unshare(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// GET /api/session/{sessionID}/event — durable SSE for one session. ocserve
/// has no per-session event stream distinct from the global bus; alias the
/// global SSE (frames carry sessionID, clients filter). Semantics are a
/// subset; wire shape (text/event-stream) matches.
pub async fn session_event(State(st): State<Arc<AppState>>) -> axum::response::Response {
    crate::global_event(State(st)).await
}

/// POST /api/session/{sessionID}/permission — create a permission request.
/// ocserve's gate is a rendezvous for the RUNNING prompt; a standalone
/// create is acknowledged with the evaluated effect (freeze shape
/// `{data:{id,effect}}`). Full blocking semantics belong to the prompt path.
pub async fn permission_create(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let action = body.get("action").and_then(|v| v.as_str());
    let resources = body.get("resources").and_then(|v| v.as_array());
    let (Some(action), Some(resources)) = (action, resources) else {
        let key = if action.is_none() {
            "action"
        } else {
            "resources"
        };
        return Err(payload_err(key));
    };
    if let Some(sv) = body.get("save").filter(|v| !v.is_null() && !v.is_array()) {
        // probe-pinned bytes (save:false → freeze v2 envelope); null = absent
        return Err(HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({
                "message": format!("Expected array, got {sv}\n  at [\"save\"]"),
                "kind": "Payload",
            }),
        });
    }
    // The v2 create IS the permission oracle: it returns the evaluated effect
    // (upstream PermissionV2.ask) with NO tool execution. It must evaluate the
    // session agent's ruleset — the previous hardcoded `effect:"allow"` was
    // semantically empty (found live 2026-10-09: freeze edit /etc/passwd → ask,
    // ocserve → allow). Agent precedence: payload `agent` → session stored →
    // default. Combined effect: any deny → deny, else any ask → ask, else allow
    // (upstream evaluateInput).
    let agent = body
        .get("agent")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| session_agent(&st, &id));
    let rules = crate::agent_rules(&st, &agent);
    let mut effect = "allow";
    for r in resources {
        let Some(res) = r.as_str() else { continue };
        match ocserve_tools::evaluate(action, res, &rules).as_str() {
            "deny" => {
                effect = "deny";
                break;
            }
            "ask" if effect != "deny" => effect = "ask",
            _ => {}
        }
    }
    let pid = format!("per_{}", ocserve_core::ids::ascending_tail());
    // `ask` registers a real pending request (upstream `create`); other effects
    // do not. The request body mirrors the v1 event shape so GET /permission
    // and the reply routes see it.
    if effect == "ask" {
        let request = json!({
            "id": pid,
            "sessionID": id,
            "action": action,
            "resources": resources,
            "patterns": resources,
            "always": resources,
            "metadata": body.get("metadata").cloned().unwrap_or(json!({})),
        });
        st.gate.register_persistent(&pid, request);
    }
    Ok(Json(json!({"data": {"id": pid, "effect": effect}})))
}

/// Session's stored agent, else the runtime default. Read-only, best-effort.
fn session_agent(st: &Arc<AppState>, session_id: &str) -> String {
    if let Ok(conn) = ocserve_store::pragma::open_reader(&st.db)
        && let Ok(Some(raw)) = conn.query_row(
            "SELECT agent FROM session WHERE id = ?1",
            [session_id],
            |r| r.get::<_, Option<String>>(0),
        )
        && !raw.is_empty()
    {
        return raw;
    }
    st.llm.read().default_agent.clone()
}

/// DELETE /experimental/worktree — 400 WorktreeRemoveInput required.
pub async fn worktree_remove() -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({"name": "BadRequest", "data": {"message": "Expected WorktreeRemoveInput, got undefined", "kind": "Payload"}}),
        ),
    )
}

/// PATCH /project/{projectID} — echo the project (freeze returns the project
/// record; updates to name/icon are not persisted by ocserve).
pub async fn patch_project(Path(id): Path<String>) -> impl IntoResponse {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Json(json!({
        "id": id,
        "worktree": "/",
        "time": {"created": now_ms, "updated": now_ms, "initialized": now_ms},
        "sandboxes": [],
    }))
}

/// POST /sync/start — `true` (freeze: workspace sync started).
pub async fn sync_start() -> impl IntoResponse {
    Json(json!(true))
}

/// POST /project/git/init — Project record (freeze shape). ocserve does not
/// initialize a git repo; it echoes the current project with `vcs:"git"`
/// when the worktree is a git checkout (probed 2026-10-09).
pub async fn git_init(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let worktree = st.paths["directory"].as_str().unwrap_or("/").to_string();
    Json(json!({
        "id": "global",
        "worktree": worktree,
        "vcs": "git",
        "time": {"created": now_ms, "updated": now_ms, "initialized": now_ms},
        "sandboxes": [],
    }))
}

/// PATCH /api/credential/{credentialID} — NoContent (no credential store;
/// ocserve has no OAuth credential backend — named divergence D-CREDENTIAL).
pub async fn credential_update(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// DELETE /api/credential/{credentialID} — NoContent.
pub async fn credential_remove(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// POST /api/integration/{integrationID}/connect/key — NoContent (no
/// credential backend; named divergence D-CREDENTIAL).
pub async fn integration_connect_key(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// POST /api/integration/{integrationID}/connect/oauth — 401 (no OAuth
/// backend configured; freezes probe returns UnauthorizedError).
pub async fn integration_connect_oauth(Path(_id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"_tag": "UnauthorizedError", "message": "No OAuth credentials available"})),
    )
}

/// GET /api/integration/attempt/{attemptID} — 404 (no attempts).
pub async fn integration_attempt_get(Path(id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"_tag": "InvalidRequestError", "message": format!("Attempt not found: {id}")})),
    )
}

/// DELETE /api/integration/attempt/{attemptID} — 404 (no attempts).
pub async fn integration_attempt_delete(Path(id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"_tag": "InvalidRequestError", "message": format!("Attempt not found: {id}")})),
    )
}

/// POST /api/integration/attempt/{attemptID}/complete — 404 (no attempts).
pub async fn integration_attempt_complete(Path(id): Path<String>) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"_tag": "InvalidRequestError", "message": format!("Attempt not found: {id}")})),
    )
}

/// POST /global/upgrade — 400 BadRequest (missing target). ocserve does not
/// self-upgrade; cited divergence.
pub async fn global_upgrade(Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("target").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"target\"]", "kind": "Payload"}})),
        )
            .into_response();
    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"name": "BadRequest", "data": {"message": "Upgrade is not supported", "kind": "Payload"}})),
    )
        .into_response()
}

/// POST /sync/replay — `true`.
pub async fn sync_replay(Json(_body): Json<Value>) -> impl IntoResponse {
    Json(json!(true))
}

/// POST /sync/history — `{data:[]}`.
pub async fn sync_history(Json(_body): Json<Value>) -> impl IntoResponse {
    Json(json!({"data": []}))
}

/// POST /sync/steal — `true`.
pub async fn sync_steal(Json(_body): Json<Value>) -> impl IntoResponse {
    Json(json!(true))
}

/// GET /skill — bare array (v1 shape; distinct from /api/skill's envelope).
pub async fn list_skills_raw(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let payloads = st.payloads.read();
    let data: Vec<Value> = payloads
        .command
        .iter()
        .filter(|c| c.get("source").and_then(|v| v.as_str()) == Some("skill"))
        .map(|c| {
            json!({
                "name": c.get("name").cloned().unwrap_or(Value::Null),
                "description": c.get("description").cloned().unwrap_or(json!("")),
                "location": "",
                "content": c.get("template").cloned().unwrap_or(json!("")),
            })
        })
        .collect();
    Json(data)
}

/// POST /vcs/apply — apply a patch. ocserve does not mutate the worktree
/// (write path; D-VCS-NOAPPLY). Freeze-probed error shapes:
/// missing `patch` → 400 payload; otherwise `VcsApplyError` "not-clean".
pub async fn vcs_apply(Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("patch").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"patch\"]", "kind": "Payload"}})),
        )
            .into_response();
    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"name": "VcsApplyError", "data": {"message": "Patch can't be applied", "reason": "not-clean"}})),
    )
        .into_response()
}

/// POST /experimental/workspace — create a workspace. Freeze requires `type`
/// and rejects unknown adapters. ocserve registers the built-in adapters and
/// mirrors the error envelope for others.
pub async fn workspace_create(Json(body): Json<Value>) -> impl IntoResponse {
    let Some(wtype) = body.get("type").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"type\"]", "kind": "Payload"}})),
        )
            .into_response();
    };
    let known = matches!(wtype, "worktree" | "local" | "directory");
    if !known {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "WorkspaceCreateError", "data": {"message": format!("Unknown workspace adapter: {wtype}")}})),
        )
            .into_response();
    }
    Json(json!({"id": format!("wrk_{}", ocserve_core::ids::ascending_tail()), "type": wtype}))
        .into_response()
}

/// POST /api/session — create a session (v2). Shares the v1 creation path.
pub async fn create_session(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let resolved = crate::resolve_create_dir_path(&st, &body, None).await;
    let info = crate::create_session_record(&st, &body, resolved)?;
    Ok(Json(json!({"data": info})))
}
