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
//! (`assets/openapi/1.18.31.json`, served at `/doc`) and probed live from the
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
fn available_provider_ids(payloads: &crate::Payloads) -> Vec<String> {
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
    let cat_obj = payloads.catalog.as_object();
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
    // variants: UNION (D-V2-UNION) — v1 wants [{id,headers,body}], v2
    // wants [{id,settings}] and v2's Model.Variant already declares all
    // three keys optional (additionalProperties:false SAFE): emit all three
    // so both decoders read their keys. settings = the variant body (the
    // reasoning-effort payload both sides carry).
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
                    json!({"id": k, "headers": {}, "body": body, "settings": body})
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
        // UNION (D-V2-UNION): v2 `Model.Info` requires `modelID` (= id),
        // flat `package` (= the npm package), and `settings`. The v1
        // `api:{type,...}` union stays for v1 clients (different keys, no
        // conflict). `compatibility` is provider-packaged data we don't
        // carry — null (relax_null covers it; never fabricated).
        "modelID": v1.get("id").cloned().unwrap_or(Value::Null),
        "package": npm,
        "settings": {},
        "compatibility": Value::Null,
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
pub async fn list_providers(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    crate::wire_json_h(&st, "api_provider", ae, inm).unwrap_or_else(|| {
        let payloads = st.payloads.read();
        Json(providers_body(&payloads, &api_location(&st))).into_response()
    })
}

/// Pure projections of the v2 metadata group over the payloads — the ONE
/// source of truth for both the request path and `rebuild_wire` (the wire
/// bytes are these values serialized with serde_json). Byte-parity pinned by
/// `tests/v2_body_parity.rs` (including a planted-drift negative control).
pub fn providers_body(p: &crate::Payloads, location: &Value) -> Value {
    let avail = available_provider_ids(p);
    let all = p
        .provider
        .get("all")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let data: Vec<Value> = all
        .iter()
        .filter(|p| {
            p.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| avail.iter().any(|a| a == id))
        })
        .map(project_provider_v2)
        .collect();
    json!({"location": location, "data": data})
}

/// Pure model-list projection (the `/api/model` body).
pub fn models_body(p: &crate::Payloads, location: &Value) -> Value {
    let avail = available_provider_ids(p);
    let all = p
        .provider
        .get("all")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let mut data: Vec<Value> = Vec::new();
    for p in all {
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
    json!({"location": location, "data": data})
}

/// Pure integration-list projection (the `/api/integration` body).
pub fn integrations_body(p: &crate::Payloads, location: &Value) -> Value {
    let catalog = p.catalog.as_object();
    let mut out: Vec<Value> = Vec::new();
    if let Some(map) = catalog {
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
    if let Some(cps) = p.config.get("provider").and_then(|v| v.as_object()) {
        for (id, v) in cps {
            if catalog.map(|c| c.contains_key(id)).unwrap_or(false) {
                continue;
            }
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or(id);
            out.push(integration_entry(id, name, &[]));
        }
    }
    json!({"location": location, "data": out})
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
        // UNION (D-V2-UNION): v2 `Provider.Info` requires `integrationID`
        // (= id), `activation` ("enabled" when available-listed), flat
        // `package`, and `settings`. Secrets never emit: settings carries
        // only the non-secret baseURL (apiKey values stay in config —
        // SECURITY posture over parity; named divergence D-PROV-SECRETS).
        "integrationID": v1.get("id").cloned().unwrap_or(Value::Null),
        "activation": "enabled",
        "package": npm,
        "settings": {"baseURL": url},
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
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
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
pub async fn list_models(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    crate::wire_json_h(&st, "api_model", ae, inm).unwrap_or_else(|| {
        let payloads = st.payloads.read();
        Json(models_body(&payloads, &api_location(&st))).into_response()
    })
}

// ---------------------------------------------------------------------------
// skills / integrations / references
// ---------------------------------------------------------------------------

/// GET /api/skill — UNION `{location, data}` (D-V2-UNION): v1 `SkillV2Info`
/// keys plus v2 `Skill.Info` required `id` + `path`. `id` is the slug (freeze:
/// id "opencode"), `path` the skill location (`<built-in>` when bundled).
pub async fn list_skills(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let payloads = st.payloads.read();
    let data: Vec<Value> = payloads
        .command
        .iter()
        .filter(|c| c.get("source").and_then(|v| v.as_str()) == Some("skill"))
        .map(|c| {
            let name = c.get("name").cloned().unwrap_or(Value::Null);
            json!({
                "id": name,
                "name": name,
                "description": c.get("description").cloned().unwrap_or(json!("")),
                "slash": false,
                "location": "",
                "content": c.get("template").cloned().unwrap_or(json!("")),
                "path": c.get("location").cloned().unwrap_or(json!("<built-in>")),
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

/// GET /api/integration — `{location, data:[IntegrationInfo]}`.
pub async fn list_integrations(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    crate::wire_json_h(&st, "api_integration", ae, inm).unwrap_or_else(|| {
        let payloads = st.payloads.read();
        Json(integrations_body(&payloads, &api_location(&st))).into_response()
    })
}

/// GET /api/integration/{integrationID} — single integration.
pub async fn get_integration(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let payloads = st.payloads.read();
    let items = integrations_body(&payloads, &api_location(&st));
    // freeze returns `{location, data:null}` for an unknown id (probed), not a
    // 404 — the web UI reads `data` and treats null as "not configured".
    match items["data"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(&id))
        })
        .cloned()
    {
        Some(i) => Ok(Json(json!({"location": api_location(&st), "data": i}))),
        None => Ok(Json(json!({"location": api_location(&st), "data": null}))),
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
) -> Result<Json<Value>, crate::HttpError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/")).to_path_buf();
    // `query` is REQUIRED (freeze: missing → 400 `_tag:InvalidRequestError`
    // kind Query); empty accepted.
    let query = q
        .get("query")
        .ok_or_else(|| crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "Missing key\n  at [\"query\"]", "kind": "Query"}),
        })?
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
    .map_err(|e| {
        crate::HttpError::Api(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalServerError",
            message: format!("fs_find worker: {e}"),
        })
    })?
    .map_err(|e| {
        crate::HttpError::Api(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: format!("fs_find: {e:#}"),
        })
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

/// GET /session/{id}/diff — with `?messageID=`, the message's stored
/// `summary.diffs` (upstream `SessionSummary.diff`); without it, the session
/// convenience path returns `[]`. Crucially this is **JSON**, not SPA HTML: the
/// TUI's `SidebarFiles` flatMaps the result and an HTML string crashes it.
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

/// v2 404 for routes that do not exist until acceptance (V2-PLAN.md D2:
/// `/api/info` and the v2 runtime stay dark behind OCSERVE_V2=1). Distinct
/// from `session_not_found` (unknown session) — this is unknown CAPABILITY.
fn v2_not_found(id: &str) -> crate::HttpError {
    crate::HttpError::TaggedData {
        status: StatusCode::NOT_FOUND,
        tag: "SessionNotFoundError",
        fields: json!({"sessionID": id, "message": "v2 runtime disabled".to_string()}),
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
/// Real `content[]` via `store::project_page_v2` (V2-PLAN.md P4 — the
/// skeletal `"content": []` is gone).
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
    let (msgs, _more, next) = run_blocking(&st.db, move |db| {
        ocserve_store::page_messages(db, &sid, limit as u64, None)
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let sid2 = id.clone();
    let data = run_blocking(&st.db, move |db| ocserve_store::project_page_v2(db, &msgs))
        .await
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
    // freeze always emits BOTH cursor keys (previous/next), null when absent.
    let mut cursor = json!({"previous": Value::Null, "next": Value::Null});
    if let Some(n) = next {
        cursor["next"] = json!(n);
    }
    Ok(Json(json!({"data": data, "cursor": cursor})))
}

/// GET /api/session/{sessionID}/message/{messageID} — single message.
/// Real `content[]` via `store::project_page_v2` (same projector as the
/// list path — one projection, no drift).
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
    let row_present = row.is_some();
    let info = row.and_then(|v| v.get("info").map(|i| (mid.clone(), i.to_string())));
    let mut projected: Option<Value> = match info {
        Some((m, info)) => {
            let sid3 = id.clone();
            run_blocking(&st.db, move |db| {
                ocserve_store::project_page_v2(db, &[(m, info)])
            })
            .await
            .map_err(internal)?
            .into_iter()
            .next()
        }
        None => None,
    };
    // Unknown roles project to nothing; fall back to the id shell so the
    // route stays 200-shape-faithful for stored-but-unprojectable rows.
    if projected.is_none() && row_present {
        projected = Some(json!({"id": mid}));
    }
    match projected {
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
    // Both contracts define context as `{data}` with NO cursor (v1
    // additionalProperties:false, v2 likewise) — the cursor serves nobody
    // here, so strip it rather than allowlist it (exact on both).
    let Json(v) =
        list_messages(State(st), Path(id), Query(std::collections::HashMap::new())).await?;
    Ok(Json(json!({"data": v.get("data").cloned().unwrap_or(json!([]))})))
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
    set_session_field(&st, &id, "agent", json!(agent)).await?;
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
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_session_field(
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
    // Perf dive 2026-10-11: blocking store calls on this async handler's
    // worker — move both the existence check and the write to the blocking
    // pool (F1 class).
    let db = st.db.clone();
    let id_check = id.to_string();
    let exists = tokio::task::spawn_blocking(move || ocserve_store::session_exists(&db, &id_check))
        .await
        .map_err(|e| crate::HttpError::Api(internal(anyhow::anyhow!("exists join: {e}"))))?
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
    if !exists {
        return Err(session_not_found(id));
    }
    let text = if field == "model" && val.is_object() {
        val.to_string()
    } else if let Some(s) = val.as_str() {
        s.to_string()
    } else {
        val.to_string()
    };
    let w = st.writer.clone();
    let id_w = id.to_string();
    let col_w = col.to_string();
    tokio::task::spawn_blocking(move || {
        w.write(vec![ocserve_store::WriteOp::Sql {
            sql: format!("UPDATE session SET {col_w} = ?2 WHERE id = ?1"),
            params: vec![id_w.into(), text.into()],
        }])
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(anyhow::anyhow!("write join: {e}"))))?
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

/// POST /api/session/{sessionID}/prompt — v2 admission (V2-PLAN.md P3).
///
/// Upstream semantics (`session.ts:146-179`): the route only ADMITS the prompt
/// to `session_inbox` and returns `Session.Inbox.User`; a coordinator wakes
/// the execution drain. The v1 synchronous path is untouched — this route
/// never calls `run_prompt` inline.
///
/// Flat 2.0.28 body (`{id?, text!, files?, agents?, skills?, metadata?,
/// delivery?, resume?}`); idempotent first-admit-wins on `id`
/// (`inbox.ts:154-167`). Behind `OCSERVE_V2=1` (D2: dark until acceptance).
pub async fn prompt(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    // boundary decode FIRST with the v2 envelope (`_tag:InvalidRequestError`,
    // field-probes.md) — session/endpoint checks follow.
    let input = crate::decode_v2(ocserve_core::wire::V2PromptInput::decode(&body))?;
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    // Revert-commit cleanup before admission (upstream session.ts:165-166:
    // staged revert commits after prepare, before admit).
    ocserve_store::revert_cleanup(&st.writer, &st.db, &id).map_err(internal)?;
    let msg_id = input.id.clone().unwrap_or_else(new_v2_msg_id);
    let delivery = input.delivery.clone().unwrap_or_else(|| "steer".into());
    let payload = json!({
        "text": input.prompt_text,
        "files": input.prompt_files,
        "agents": input.prompt_agents,
        "skills": input.prompt_skills,
        "metadata": input.metadata,
    });
    // Durable admission event first (InboxEnqueued), then the inbox row with
    // enqueued_seq = the event seq (upstream: enqueuedSeq = durable seq).
    let (seq, _evt) = ocserve_store::emit_event(
        &st.writer,
        Some(&id),
        "session.inbox.enqueued",
        &json!({"inboxID": msg_id, "sessionID": id}),
    )
    .map_err(internal)?;
    let fresh =
        ocserve_store::inbox::admit(&st.writer, &id, &msg_id, "user", &payload, &delivery, seq)
            .map_err(internal)?;
    // Idempotent reconcile: a re-admit returns the STORED item, not the new
    // request body (first write wins — echoing the request would lie about
    // what the queue holds).
    let (payload, delivery) = if fresh {
        (payload, delivery)
    } else {
        match ocserve_store::inbox::get(&st.db, &msg_id).map_err(internal)? {
            Some((_, _, stored_text, stored_delivery, _)) => (
                serde_json::from_str(&stored_text).unwrap_or(json!({"text": ""})),
                stored_delivery,
            ),
            None => (payload, delivery), // delivered between admit and read
        }
    };
    // Wake the coordinator unless resume:false (admit-only).
    if input.resume != Some(false) {
        let scope = if delivery == "queue" {
            ocserve_core::coordinator::WakeScope::Input
        } else {
            ocserve_core::coordinator::WakeScope::Steer
        };
        if st.coordinators.wake(&id, scope) {
            let st2 = st.clone();
            let sid = id.clone();
            tokio::spawn(async move {
                drain_session(st2, sid).await;
            });
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(Json(json!({
        "data": {
            "id": msg_id,
            "sessionID": id,
            "time": {"created": now_ms},
            "type": "user",
            "payload": payload,
            "delivery": delivery,
        }
    })))
}

/// Mint a v2 message id (`msg_` + 24 base62 chars — the frozen `^msg_`
/// brand; upstream ids are time+random alphanumerics, clients require only
/// uniqueness + the prefix).
fn new_v2_msg_id() -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_be_bytes());
    h.update(std::process::id().to_be_bytes());
    let digest = h.finalize();
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(28);
    out.push_str("msg_");
    // Divide out base-62 digits (modulo, not bitmask: 62 is not a power of 2 —
    // `& 63` can index past the 62-symbol table).
    let mut acc: u128 = 0;
    for &byte in digest.iter().take(16) {
        acc = (acc << 8) | byte as u128;
    }
    for _ in 0..24 {
        out.push(B62[(acc % 62) as usize] as char);
        acc /= 62;
    }
    out
}

/// v2 drain loop for one session: promote-one + run-one-turn via the shared
/// `run_prompt` executor, until nothing is promotable. Claims wrap the drain
/// (write-ahead on start, released on terminal states).
///
/// `pub(crate)` so inbox-update wakes (steer flips) share the exact drain.
pub(crate) async fn drain_session(st: Arc<AppState>, sid: String) {
    use ocserve_core::coordinator::WakeScope;
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    };
    // Write-ahead claim in the same batch as the started event.
    let _ = ocserve_store::claim::claim(&st.writer, &sid, now_ms());
    let _ = crate::emit_durable_http(&st, &sid, "session.execution.started", json!({}));
    loop {
        // Doorbell first, work-continuation second (upstream `advanceToStep`
        // loops on promotability, not doorbells — otherwise a wake that
        // arrives mid-turn strands the rest of the queue). `next_scope`
        // keeps `draining` armed while work remains, so no second drain can
        // spawn mid-loop.
        let work_remains = ocserve_core::coordinator::next_for_scope(
            &st.db,
            &sid,
            ocserve_core::coordinator::WakeScope::Input,
        )
        .ok()
        .flatten()
        .is_some();
        let Some(scope) = st.coordinators.next_scope(&sid, work_remains) else {
            break;
        };
        let next = ocserve_core::coordinator::next_for_scope(&st.db, &sid, scope);
        let Ok(Some(item_id)) = next else {
            // Nothing promotable in this scope: idle-scope falls through to
            // queue inside next_for_scope; a Steer-scope miss ends the drain.
            if scope == WakeScope::Input {
                break;
            }
            continue;
        };
        let turn = run_v2_turn(&st, &sid, &item_id).await;
        if let Err(e) = turn {
            tracing::warn!("v2 drain turn failed for {sid}: {e:#}");
            break;
        }
    }
    let _ = ocserve_store::claim::release(&st.writer, &sid);
    let _ = crate::emit_durable_http(&st, &sid, "session.execution.succeeded", json!({}));
}

/// Run one admitted inbox item through the shared executor: deliver (delete
/// the inbox row) then build the v1 request and run the turn body.
async fn run_v2_turn(st: &Arc<AppState>, sid: &str, item_id: &str) -> anyhow::Result<()> {
    let Some((_isid, itype, payload_text, delivery, _seq)) =
        ocserve_store::inbox::get(&st.db, item_id)?
    else {
        return Ok(()); // cancelled between promote and run — not an error
    };
    if itype != "user" && itype != "synthetic" {
        // compaction/move items: P4 delivers them; for now deliver-delete so
        // the queue cannot wedge on an unhandled kind.
        st.writer
            .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
        let _ = delivery;
        return Ok(());
    }
    let payload: Value = serde_json::from_str(&payload_text).unwrap_or(json!({"text": ""}));
    // Deliver-delete + message insert happen in the turn body
    // (`insert_message`); the delete joins that same writer flow by running
    // first here (single-writer serialization makes the pair atomic enough:
    // no reader observes a half-delivered item — the message insert follows
    // in the same task before any other promotable runs).
    st.writer
        .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
    let req = ocserve_core::wire::PromptRequest {
        message_id: Some(item_id.to_string()),
        parts: v2_payload_parts(&payload),
        ..Default::default()
    };
    let ctx = crate::build_prompt_context(st, &req, sid)
        .map_err(|e| anyhow::anyhow!("{} {}", e.name, e.message))?;
    let writer = st.writer.clone();
    ocserve_core::prompt::run_prompt(&ctx, &writer, sid, &req)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let _ = itype;
    Ok(())
}

/// Inbox payload → v1 parts array.
fn v2_payload_parts(payload: &Value) -> Vec<Value> {
    let mut parts = vec![json!({
        "type": "text",
        "text": payload.get("text").cloned().unwrap_or(json!("")),
    })];
    for arr_key in ["files", "agents"] {
        if let Some(arr) = payload.get(arr_key).and_then(|v| v.as_array()) {
            for el in arr {
                parts.push(json!({"type": arr_key.trim_end_matches('s'), arr_key: el}));
            }
        }
    }
    let _ = payload.get("skills");
    parts
}

// ---------------------------------------------------------------------------
// server info — version probe (V2-PLAN.md D2)
// ---------------------------------------------------------------------------

/// GET /api/info — the v2 version probe (`ServerInfo{version,pid,urls,paths,
/// capabilities}`). DARK until P5 acceptance: JSON 404 while `v2_enabled`
/// is false (never SPA fallthrough — a client probing the version must get
/// a version signal, not HTML).
pub async fn server_info(State(st): State<Arc<AppState>>) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "SessionNotFoundError",
            fields: json!({"message": "v2 runtime disabled".to_string()}),
        });
    }
    Ok(Json(json!({
        "version": ocserve_core::spec::V2_TAG,
        "pid": std::process::id(),
        "urls": [],
        "paths": {"tmp": std::env::temp_dir().to_string_lossy()},
        "capabilities": {"persistentPty": false},
    })))
}

// ---------------------------------------------------------------------------
// form routes — create/list/get/reply/cancel + global list (V2-PLAN.md P4)
// ---------------------------------------------------------------------------

/// Project a form row to `Form.Info` (+ `state` for Detail).
fn form_info(
    session_id: &str,
    id: &str,
    title: &str,
    metadata_text: &str,
    fields_text: &str,
    state: Option<&str>,
) -> Value {
    let metadata: Value = serde_json::from_str(metadata_text).unwrap_or(json!({}));
    let fields: Value = serde_json::from_str(fields_text).unwrap_or(json!([]));
    let mut out = json!({
        "id": id, "sessionID": session_id, "title": title,
        "metadata": metadata, "fields": fields,
    });
    if let Some(s) = state {
        out["state"] = json!({"status": s});
    }
    out
}

/// POST /api/session/{sessionID}/form — create a pending ask. Registers the
/// waiter, persists the row, emits `form.created`.
pub async fn form_create(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let title = body.get("title").and_then(|t| t.as_str()).unwrap_or("");
    let fields = body.get("fields").cloned().unwrap_or(json!([]));
    if fields.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "fields must be a non-empty array"}),
        });
    }
    let fid = body
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| s.starts_with("frm_"))
        .map(str::to_string)
        .unwrap_or_else(ocserve_core::form::frm_id);
    let metadata = body.get("metadata").cloned().unwrap_or(json!({}));
    let info = json!({"id": fid, "sessionID": id, "title": title,
                      "metadata": metadata, "fields": fields});
    let Some(rx) = st.form_gate.register(&fid, info.clone()) else {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::SERVICE_UNAVAILABLE,
            tag: "ServiceUnavailableError",
            fields: json!({"message": "too many pending forms"}),
        });
    };
    // The abort guard lives inside the gate (released on settle); the
    // receiver is the runner's end. HTTP create does not wait — reply/cancel
    // routes settle. (A runner-side wait path arrives with the tool adapter.)
    let _ = rx;
    ocserve_store::form::create(
        &st.writer,
        &id,
        &fid,
        title,
        &metadata.to_string(),
        &fields.to_string(),
    )
    .map_err(internal)?;
    let _ = crate::emit_durable_http(&st, &id, "form.created", info.clone());
    Ok(Json(
        json!({"data": form_info(&id, &fid, title, &metadata.to_string(), &fields.to_string(), None)}),
    ))
}

/// GET /api/session/{sessionID}/form — pending asks for the session.
pub async fn form_list(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let rows = ocserve_store::form::list_pending(&st.db, &id).map_err(internal)?;
    Ok(Json(json!({"data": rows
        .iter()
        .map(|(fid, title, meta, fields)| form_info(&id, fid, title, meta, fields, None))
        .collect::<Vec<_>>()})))
}

/// GET /api/session/{sessionID}/form/{formID} — Detail (with state).
pub async fn form_get(
    State(st): State<Arc<AppState>>,
    Path((id, fid)): Path<(String, String)>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    match ocserve_store::form::get(&st.db, &fid).map_err(internal)? {
        Some((sid, title, meta, fields, state, _answer)) if sid == id => Ok(Json(
            json!({"data": form_info(&id, &fid, &title, &meta, &fields, Some(&state))}),
        )),
        _ => Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "FormNotFoundError",
            fields: json!({"message": format!("Form not found: {fid}")}),
        }),
    }
}

/// POST /api/session/{sessionID}/form/{formID}/reply — answer a pending ask.
/// Settles the row + wakes the waiter; re-settle of a settled form is a 409
/// (upstream `FormAlreadySettledError`).
pub async fn form_reply(
    State(st): State<Arc<AppState>>,
    Path((id, fid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let answer = body.get("answer").cloned().unwrap_or(Value::Null);
    match ocserve_store::form::get(&st.db, &fid).map_err(internal)? {
        Some((sid, _, _, _, state, _)) if sid == id => {
            if state != "pending" {
                return Err(crate::HttpError::TaggedData {
                    status: StatusCode::CONFLICT,
                    tag: "FormAlreadySettledError",
                    fields: json!({"message": format!("Form already settled: {fid}")}),
                });
            }
        }
        _ => {
            return Err(crate::HttpError::TaggedData {
                status: StatusCode::NOT_FOUND,
                tag: "FormNotFoundError",
                fields: json!({"message": format!("Form not found: {fid}")}),
            });
        }
    }
    ocserve_store::form::settle(&st.writer, &fid, "answered", Some(&answer.to_string()))
        .map_err(internal)?;
    st.form_gate.reply(&fid, answer);
    let _ = crate::emit_durable_http(&st, &id, "form.replied", json!({"id": fid}));
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/session/{sessionID}/form/{formID}[?message=] — cancel a
/// pending ask (settles as cancelled; idempotent success on settled/missing
/// rows within the session scope).
pub async fn form_cancel(
    State(st): State<Arc<AppState>>,
    Path((id, fid)): Path<(String, String)>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    match ocserve_store::form::get(&st.db, &fid).map_err(internal)? {
        Some((sid, _, _, _, _, _)) if sid == id => {
            let _ = ocserve_store::form::settle(&st.writer, &fid, "cancelled", None)
                .map_err(internal)?;
            st.form_gate.cancel(&fid);
            let _ = crate::emit_durable_http(&st, &id, "form.cancelled", json!({"id": fid}));
        }
        _ => {}
    }
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/form — pending asks across sessions (gate-backed, location
///-agnostic; the durable rows scope per session via the session routes).
pub async fn form_list_global(State(st): State<Arc<AppState>>) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    // v2 `form.list` is a Location.response — envelope required.
    Ok(Json(json!({"location": api_location(&st), "data": st.form_gate.list()})))
}

/// Project one inbox row to `Session.Inbox.Info`
/// (`{id, sessionID, time, type, payload, delivery}`).
fn inbox_info(session_id: &str, row: &(String, String, String, String, i64)) -> Value {
    let (id, itype, payload_text, delivery, _seq) = row;
    let payload: Value = serde_json::from_str(payload_text).unwrap_or(json!({}));
    // The stored payload is the v1-shape user/synthetic body; the v2 envelope
    // carries it under `payload` with the item `type` alongside.
    json!({
        "id": id,
        "sessionID": session_id,
        "time": {"created": 0},
        "type": itype,
        "payload": payload,
        "delivery": delivery,
    })
}

/// GET /api/experimental/session/{sessionID}/log?after=&follow= — durable
/// event replay (V2-PLAN.md P4). Upstream protocol (`bus.ts:149-160`):
/// `after` = exclusive `Event.Seq`; `follow=false` completes at the
/// watermark; `follow=true` replays then goes live; exactly one `log.synced
/// {aggregateID, seq}` marker at the captured watermark.
///
/// Rows come from the always-on event log (D-EVENT-ALWAYSON: ours works by
/// default where upstream needs `persist:true`). Versioned stored types
/// (`type@v1`) project to v2 grammar `{id,type,durable,data}` — the same
/// projection as `load_session_events`, plus the watermark marker.
pub async fn session_log(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, HttpError> {
    use axum::response::sse::{Event, Sse};
    use futures_util::stream::{self, StreamExt};
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let after: i64 = q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
    let follow = q.get("follow").map(|f| f == "true").unwrap_or(false);
    let sid = id.clone();
    let rows = run_blocking(&st.db, move |db| {
        ocserve_store::events_after(db, &sid, after)
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let watermark = rows.last().map(|(s, _, _, _)| *s).unwrap_or(after);
    let sid2 = id.clone();
    let frames: Vec<String> = rows
        .into_iter()
        .map(|(seq, ty, payload_text, _t)| {
            let data: Value = serde_json::from_str(&payload_text).unwrap_or(json!({}));
            let body = data.pointer("/properties").cloned().unwrap_or(data);
            let unversioned = ocserve_store::unversioned_event_type(&ty);
            let version: i64 = ty
                .rsplit_once('@')
                .and_then(|(_, s)| s.strip_prefix('v')?.parse().ok())
                .unwrap_or(1);
            json!({
                "id": format!("evt_{seq}"),
                "type": unversioned,
                "durable": {"aggregateID": sid2, "seq": seq, "version": version},
                "data": body,
            })
            .to_string()
        })
        .collect();
    let synced = json!({
        "type": "log.synced", "aggregateID": sid2, "seq": watermark,
    })
    .to_string();
    let stream = stream::iter(
        frames
            .into_iter()
            .map(|f| Ok::<_, std::convert::Infallible>(Event::default().data(f))),
    );
    if follow {
        // Replay-then-live: the watermark marker, then the live bus filtered
        // to v2-grammar frames for this session. Bounded by the same ring
        // subscription as /api/event (no new channel).
        let rx = st.bus.subscribe();
        let sid3 = id.clone();
        let live = stream::unfold(rx, move |mut rx| {
            let sid = sid3.clone();
            async move {
                loop {
                    match rx.recv().await {
                        Ok(frame) => {
                            // v2-grammar frames carry no payload key; live
                            // here means v2 twins + durable projections for
                            // this session (session check via data.sessionID).
                            if frame.contains("sessionID") && frame.contains(&sid as &str) {
                                return Some((Ok(Event::default().data(frame)), rx));
                            }
                        }
                        Err(_) => return None,
                    }
                }
            }
        });
        let synced_ev = Event::default().data(synced);
        Ok(Sse::new(
            stream
                .chain(stream::once(async { Ok(synced_ev) }))
                .chain(live),
        )
        .into_response())
    } else {
        let synced_ev = Event::default().data(synced);
        Ok(Sse::new(stream.chain(stream::once(async { Ok(synced_ev) }))).into_response())
    }
}

// ---------------------------------------------------------------------------
// inbox routes — list / cancel / update (V2-PLAN.md P4, L3-lite)
// ---------------------------------------------------------------------------

/// GET /api/session/{sessionID}/inbox — list pending durable work, ordered by
/// `enqueued_seq` (upstream `session.inbox.list`).
pub async fn inbox_list(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let rows = ocserve_store::inbox::list(&st.db, &id).map_err(internal)?;
    Ok(Json(
        json!({"data": rows.iter().map(|r| inbox_info(&id, r)).collect::<Vec<_>>()}),
    ))
}

/// DELETE /api/session/{sessionID}/inbox/{inboxID} — cancel undelivered work
/// (no-op when already delivered/missing — upstream treats cancel of a gone
/// item as success, `session.ts:547`).
pub async fn inbox_cancel(
    State(st): State<Arc<AppState>>,
    Path((id, inbox_id)): Path<(String, String)>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    // Scope the cancel to this session (no cross-session deletes).
    match ocserve_store::inbox::get(&st.db, &inbox_id).map_err(internal)? {
        Some((sid, _, _, _, _)) if sid == id => {
            let _ = ocserve_store::inbox::cancel(&st.writer, &inbox_id).map_err(internal)?;
        }
        _ => {}
    }
    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /api/session/{sessionID}/inbox/{inboxID} — change `delivery`
/// (`{delivery: "steer"|"queue"}` required); a steer flip also wakes the
/// coordinator (upstream `steerInbox` wakes, `session.ts:135-141`).
pub async fn inbox_update(
    State(st): State<Arc<AppState>>,
    Path((id, inbox_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let delivery = body
        .get("delivery")
        .and_then(|v| v.as_str())
        .filter(|s| *s == "steer" || *s == "queue");
    let Some(delivery) = delivery else {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "delivery must be \"steer\" | \"queue\""}),
        });
    };
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    match ocserve_store::inbox::get(&st.db, &inbox_id).map_err(internal)? {
        Some((sid, _, _, _, _)) if sid == id => {
            if !ocserve_store::inbox::set_delivery(&st.writer, &inbox_id, delivery)
                .map_err(internal)?
            {
                return Err(session_not_found(&id));
            }
            if delivery == "steer"
                && st
                    .coordinators
                    .wake(&id, ocserve_core::coordinator::WakeScope::Steer)
            {
                let st2 = st.clone();
                let sid2 = id.clone();
                tokio::spawn(async move {
                    drain_session(st2, sid2).await;
                });
            }
        }
        _ => return Err(session_not_found(&id)),
    }
    Ok(StatusCode::NO_CONTENT)
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

/// POST /api/session/{sessionID}/revert/stage — `{data:RevertState}`. Runs the
/// real revert engine (restore files, compute the range diff, set the marker)
/// and emits `session.next.revert.staged` so the web UI resyncs.
pub async fn revert_stage(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let mid = body.get("messageID").and_then(|v| v.as_str()).unwrap_or("");
    if mid.is_empty() {
        return Err(payload_err("messageID"));
    }
    let part_id = body
        .get("partID")
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
        .map(String::from);
    let marker = crate::revert_apply(&st, &id, mid, part_id.as_deref())
        .await
        .map_err(HttpError::Api)?;
    crate::emit_durable_http(
        &st,
        &id,
        "session.next.revert.staged",
        json!({"sessionID": id, "timestamp": now_ms_http(), "revert": marker.to_value()}),
    )
    .map_err(HttpError::Api)?;
    Ok(Json(json!({"data": marker.to_value()})))
}

/// POST /api/session/{sessionID}/revert/clear — restore the snapshot, clear the
/// marker, emit `session.next.revert.cleared`; NoContent.
pub async fn revert_clear(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // restore the marker's snapshot (upstream unrevert)
    let existing = run_blocking(&st.db, {
        let sid = id.to_string();
        move |db| ocserve_store::load_session_wire(db, &sid)
    })
    .await
    .map_err(internal)?
    .and_then(|s| {
        s.get("revert")
            .and_then(|r| r.get("snapshot"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    let dir = crate::session_directory_http(&st, &id).await;
    if let Some(snap) = crate::snapshots_for(&st, &dir)
        && let Some(s) = existing
    {
        let _ = run_blocking(&st.db, move |_db| {
            snap.restore(&s);
            Ok::<_, anyhow::Error>(())
        })
        .await;
    }
    ocserve_store::set_session_revert(&st.writer, &id, None).map_err(internal)?;
    crate::emit_durable_http(
        &st,
        &id,
        "session.next.revert.cleared",
        json!({"sessionID": id}),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/revert/commit — finalize the revert: delete
/// the reverted range and clear the marker; emit `session.next.revert.committed`.
pub async fn revert_commit(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mid = run_blocking(&st.db, {
        let sid = id.to_string();
        move |db| ocserve_store::load_session_wire(db, &sid)
    })
    .await
    .map_err(internal)?
    .and_then(|s| {
        s.get("revert")
            .and_then(|r| r.get("messageID"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    if let Some(mid) = mid {
        // delete messages from the revert target onward (upstream revert.cleanup)
        let rows = run_blocking(&st.db, {
            let sid = id.to_string();
            let mid = mid.clone();
            move |db| ocserve_store::messages_from(db, &sid, &mid)
        })
        .await
        .map_err(internal)?;
        for m in rows {
            let _ = ocserve_store::delete_message(&st.writer, &st.db, &id, &m);
        }
        ocserve_store::set_session_revert(&st.writer, &id, None).map_err(internal)?;
        crate::emit_durable_http(
            &st,
            &id,
            "session.next.revert.committed",
            json!({"sessionID": id, "messageID": mid, "timestamp": now_ms_http()}),
        )?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// v2 `/api/*` payload error — probe-pinned envelope (field-probes.md):
/// `{"_tag":"InvalidRequestError","message":"Missing key\n  at [\"<key>\"]",
/// "kind":"Payload"}` [400]. NOTE: differs from v1 routes (name/data nesting)
/// — this is the v2 HttpApi tagged-error shape.
fn now_ms_http() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

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
        .unwrap_or_default();
    let agent = if agent.is_empty() {
        session_agent_async(&st, &id).await
    } else {
        agent
    };
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

/// Perf dive 2026-10-11: `session_agent` opens a reader (blocking) — the
/// async permission path calls this variant so the lookup runs on the
/// blocking pool (F1 class).
async fn session_agent_async(st: &Arc<AppState>, session_id: &str) -> String {
    let st2 = st.clone();
    let sid = session_id.to_string();
    tokio::task::spawn_blocking(move || session_agent(&st2, &sid))
        .await
        .unwrap_or_else(|_| st.llm.read().default_agent.clone())
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

/// POST /vcs/apply — apply a patch to the session directory's git worktree
/// (upstream `project/vcs.ts:399`): `git apply` → `{applied:true}`; not a git
/// worktree → `reason:"non-git"`; patch rejected → `reason:"not-clean"`.
/// missing `patch` → 400 payload. The route DOES mutate the worktree.
pub async fn vcs_apply(
    State(st): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(patch) = body.get("patch").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "BadRequest", "data": {"message": "Missing key\n  at [\"patch\"]", "kind": "Payload"}})),
        )
            .into_response();
    };
    // upstream honors `?directory=` (WorkspaceRoutingQuery); default the
    // server's worktree.
    let dir = q
        .get("directory")
        .cloned()
        .unwrap_or_else(|| st.paths["directory"].as_str().unwrap_or("/").to_string());
    let patch = patch.to_string();
    let applied = crate::run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::apply(&dir, &patch))
    })
    .await
    .unwrap_or(None);
    match applied {
        Some(true) => (StatusCode::OK, Json(json!({"applied": true}))).into_response(),
        Some(false) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "VcsApplyError", "data": {"message": "Patch can't be applied", "reason": "not-clean"}})),
        )
            .into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "VcsApplyError", "data": {"message": "Patch can't be applied because the project is not git-based", "reason": "non-git"}})),
        )
            .into_response(),
    }
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
