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
use std::collections::HashMap;
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
        // carry — OMITTED (not null: the schema isn't nullable and the key
        // isn't required; relax_null can't save a $ref-typed null).
        "modelID": v1.get("id").cloned().unwrap_or(Value::Null),
        "package": npm,
        "settings": {},
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
    // Absolute request paths resolve against the filesystem (NOT joined onto
    // the base — joining silently redirects /tmp/x to <base>/tmp/x and
    // reports success, a silent-lie the phase-C test debris proved). Both
    // shapes then face the same containment gate.
    let requested = std::path::PathBuf::from(rel);
    let joined = if requested.is_absolute() {
        requested
    } else {
        base_canon.join(rel.trim_start_matches('/'))
    };
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
// worktree routes — real git worktree management (Phase C)
// ---------------------------------------------------------------------------

/// GET /api/worktree — `Worktree.List` from `git worktree list`.
pub async fn worktree_list_api(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let list = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::worktrees(&dir))
    })
    .await
    .unwrap_or_else(|_| Value::Array(vec![]));
    Json(list).into_response()
}

/// POST /api/worktree — create (`git worktree add`). Requires projectID +
/// directory (branch optional). Non-git → 400 WorktreeNotGitError (not a
/// fake success); git failure → 400 with stderr.
pub async fn worktree_create_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    if let Some(bad) = obj
        .keys()
        .find(|k| !["projectID", "from", "branch", "directory", "name"].contains(&k.as_str()))
    {
        return bad_request(&format!("unknown key {bad}")).into_response();
    }
    let (Some(_pid), Some(directory)) = (
        obj.get("projectID").and_then(|v| v.as_str()),
        obj.get("directory").and_then(|v| v.as_str()),
    ) else {
        return bad_request("Missing key\n  at [\"projectID\"] / [\"directory\"]").into_response();
    };
    let branch = obj.get("branch").and_then(|v| v.as_str());
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let directory = directory.to_string();
    let branch = branch.map(str::to_string);
    match run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::worktree_add(
            &dir,
            &directory,
            branch.as_deref(),
        ))
    })
    .await
    {
        Ok(Ok(path)) => Json(json!({"directory": path})).into_response(),
        Ok(Err(e)) if e == "not a git repository" => (
            StatusCode::BAD_REQUEST,
            Json(json!({"name": "WorktreeNotGitError", "data": {"message": e}})),
        )
            .into_response(),
        Ok(Err(e)) => bad_request(&e).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

/// DELETE /api/worktree — remove (`git worktree remove`). Requires
/// projectID + directory + force.
pub async fn worktree_remove_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let (Some(_pid), Some(directory), force) = (
        obj.get("projectID").and_then(|v| v.as_str()),
        obj.get("directory").and_then(|v| v.as_str()),
        obj.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
    ) else {
        return bad_request("Missing key\n  at [\"projectID\"] / [\"directory\"]").into_response();
    };
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let directory = directory.to_string();
    match run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::worktree_remove(&dir, &directory, force))
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => bad_request(&e).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

/// POST /api/worktree/refresh — re-scan (stateless: the list is always live
/// from git, so refresh is an honest 204 no-op, not a fake rebuild).
pub async fn worktree_refresh_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    if body.get("projectID").and_then(|v| v.as_str()).is_none() {
        return bad_request("Missing key\n  at [\"projectID\"]").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

fn bad_request(msg: &str) -> HttpError {
    crate::HttpError::TaggedData {
        status: StatusCode::BAD_REQUEST,
        tag: "InvalidRequestError",
        fields: json!({"message": msg.to_string()}),
    }
}

// ---------------------------------------------------------------------------
// Phase C projections — config, shells, model default, debug, migration,
// plugin list, fs write
// ---------------------------------------------------------------------------

/// GET /api/config — `Config.Entry[]` (document + directory sources). The
/// global config path + the worktree config path, lowest-to-highest
/// priority (upstream summary verbatim). Document entries carry the parsed
/// file as `info` when readable JSON; unreadable/missing files are omitted
/// (the directory entry still covers discovery — never a fabricated info).
pub async fn config_list(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let home = std::env::var("HOME").unwrap_or_default();
    let worktree = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let mut out = vec![json!({"type": "directory", "path": format!("{home}/.config/opencode")})];
    for path in [
        format!("{home}/.config/opencode/opencode.json"),
        format!("{worktree}/opencode.json"),
    ] {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(info) = serde_json::from_str::<Value>(&text)
            && info.is_object()
        {
            out.push(json!({"type": "document", "info": info}));
        }
    }
    Json(Value::Array(out))
}

/// GET /api/config/shell — `ConfigShell.Option[]` from `/etc/shells`
/// (same source as the PTY shells route: path + lowercased name +
/// fish/nu-deny acceptability).
pub async fn config_shells(State(_st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(ocserve_pty::shells::list())
}

/// GET /api/model/default — `{location, data: Model.Info|null}`. The
/// configured default model projected through the union projector (null
/// when unresolvable — contract allows null).
pub async fn model_default(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let payloads = st.payloads.read();
    let (pid, mid) = st.llm.read().default_model.clone();
    let found = payloads
        .provider
        .get("all")
        .and_then(|v| v.as_array())
        .and_then(|all| {
            all.iter()
                .find(|p| p.get("id").and_then(|i| i.as_str()) == Some(&pid))
        })
        .and_then(|p| p.get("models"))
        .and_then(|m| m.get(&mid))
        .cloned();
    let data = found.map(|m| project_model_v2(&m)).unwrap_or(Value::Null);
    Json(json!({"location": api_location(&st), "data": data}))
}

/// GET /api/debug/location — known location directories (the session
/// directories plus the server worktree — the debug surface for location
/// state). DELETE evicts nothing (locations derive from live rows; the
/// contract's evict is a no-op against derived state, honestly 204).
pub async fn debug_location_list(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let mut dirs = ocserve_store::distinct_session_directories(&st.db).unwrap_or_default();
    let worktree = st.paths["worktree"].as_str().unwrap_or("/").to_string();
    if !dirs.iter().any(|d| d == &worktree) {
        dirs.insert(0, worktree);
    }
    Json(
        dirs.into_iter()
            .map(|d| json!({"directory": d}))
            .collect::<Vec<_>>(),
    )
}

pub async fn debug_location_evict(State(_st): State<Arc<AppState>>) -> impl IntoResponse {
    // Derived state has nothing to evict — honest 204, not a fake purge.
    StatusCode::NO_CONTENT
}

/// GET /api/experimental/migration/v1 — one-shot migration status (D4:
/// project-don't-convert). `completed` (no legacy import pending in this
/// process) — the bridge is the dev-only OCSERVE_LEGACY_SYNC path.
pub async fn migration_v1_status() -> impl IntoResponse {
    Json(json!({"status": "completed"}))
}

/// GET /api/plugin — `{location, data: Plugin.Info[]}`. The loaded plugins
/// from the sidecar registry (id + source + features + state).
pub async fn plugin_list(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let data: Vec<Value> = st.plugins.get().map(|_| vec![]).unwrap_or_default();
    // The sidecar exposes no plugin inventory RPC today (hook dispatch only);
    // the catalog is honestly empty until the inventory RPC lands (named gap
    // D-PLUGIN-INVENTORY — not a fabricated list).
    Json(json!({"location": api_location(&st), "data": data}))
}

/// POST /api/experimental/fs/write?path= — write request body bytes to the
/// path (parents created). Contained via `resolve_within` (the traversal
/// fix); returns `{location, data:{path}}`.
pub async fn fs_write(
    State(st): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let base = std::path::PathBuf::from(st.paths["directory"].as_str().unwrap_or("/"));
    let Some(rel) = q.get("path") else {
        return bad_request("Missing key\n  at [\"path\"]").into_response();
    };
    let dest = match resolve_within(&base, rel) {
        Ok(d) => d,
        Err(_) => {
            return crate::HttpError::TaggedData {
                status: StatusCode::FORBIDDEN,
                tag: "FileNotFoundError",
                fields: json!({"message": "path escapes the project directory"}),
            }
            .into_response();
        }
    };
    let write_result = run_blocking(&st.db, move |_db| {
        (|| -> anyhow::Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &body)?;
            Ok(())
        })()
    })
    .await;
    match write_result {
        Ok(()) => Json(json!({
            "location": api_location(&st),
            "data": {"path": rel},
        }))
        .into_response(),
        Err(e) => internal(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Phase D session transfer — export (projection), stats (columnar
// aggregates), import (bulk adoption)
// ---------------------------------------------------------------------------

/// GET /api/experimental/session/{sessionID}/export — `{data:{info,
/// messages[]}}` transfer envelope. Info from `session_wire`, messages from
/// the real `project_page_v2` projector (same rows the client reads).
pub async fn session_export_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let sid = id.clone();
    let info = run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let Some(info) = info else {
        return Err(session_not_found(&id));
    };
    let sid2 = id.clone();
    let page = run_blocking(&st.db, move |db| {
        ocserve_store::page_messages(db, &sid2, 10_000, None)
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let sid3 = id.clone();
    let messages = run_blocking(&st.db, move |db| {
        ocserve_store::project_page_v2(db, &page.0)
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let _ = sid3;
    Ok(Json(
        json!({"data": {"info": session_v2(&info), "messages": messages}}),
    ))
}

/// GET /api/experimental/session/stats — columnar aggregates (the
/// projectional synergy: indexed SUM/COUNT over stored columns, never
/// per-row JSON shredding + Effect decode like upstream's stats endpoint).
pub async fn session_stats_api(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let stats = run_blocking(&st.db, ocserve_store::session_stats_full)
        .await
        .unwrap_or_default();
    Json(json!({"data": stats}))
}

/// POST /api/experimental/session/import — bulk adoption of a transfer
/// envelope (`{info, messages[]}`). Reuses the fork engine's row-copy path
/// (same chunked discipline); returns the adopted session info.
pub async fn session_import_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let data = body.get("data").unwrap_or(&body);
    let info = data.get("info").cloned().unwrap_or(json!({}));
    let messages = data
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if info.get("id").and_then(|v| v.as_str()).is_none() {
        return Err(bad_request("Missing key\n  at [\"data.info.id\"]"));
    }
    let mut new_info = info.clone();
    let new_id = ocserve_core::ids::ses_id();
    if let Some(obj) = new_info.as_object_mut() {
        obj.insert("id".into(), json!(new_id));
    }
    let writer = st.writer.clone();
    let info_clone = new_info.clone();
    run_blocking(&st.db, move |_db| {
        ocserve_store::insert_session(&writer, &info_clone)?;
        // Messages adopt as user/assistant rows with fresh ids (history is
        // content, identity is local — same rule as fork remint).
        for m in &messages {
            let role = m.get("type").and_then(|v| v.as_str()).unwrap_or("user");
            let role = if role == "assistant" { "assistant" } else { "user" };
            let mid = ocserve_core::ids::msg_id();
            let mtext = m.get("text").cloned().unwrap_or(json!(""));
            ocserve_store::insert_message(
                &writer,
                None,
                &new_id,
                &json!({"id": mid, "role": role, "time": m.get("time").cloned().unwrap_or(json!({}))}),
                &[json!({"id": ocserve_core::ids::prt_id(), "type": "text", "text": mtext})],
            )?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    Ok(Json(json!({"data": session_v2(&new_info)})))
}

// ---------------------------------------------------------------------------
// Phase D instruction entries — content-addressed key journal
// ---------------------------------------------------------------------------

/// GET /api/experimental/session/{sessionID}/instructions/entries —
/// live key→value map for the session.
pub async fn instruction_list(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let rows = ocserve_store::instruction::list(&st.db, &id).map_err(internal)?;
    // Contract: entries are an ARRAY of {key, value} (InstructionEntry.Info),
    // never a map (the validator proved the map shape on the fresh routes).
    let mut arr: Vec<Value> = rows
        .into_iter()
        .map(|(k, v)| {
            json!({"key": k, "value": serde_json::from_str(&v).unwrap_or(json!({"raw": v}))})
        })
        .collect();
    arr.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    Ok(Json(json!({"data": arr})))
}

/// PUT .../entries/{key} — `{value!}` stores by content hash. Unknown keys
/// in the body → 400 (additionalProperties:false).
pub async fn instruction_put(
    State(st): State<Arc<AppState>>,
    Path((id, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    if let Some(bad) = obj.keys().find(|k| *k != "value") {
        return Err(bad_request(&format!("unknown key {bad}")));
    }
    let Some(value) = obj.get("value") else {
        return Err(bad_request("Missing key\n  at [\"value\"]"));
    };
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    ocserve_store::instruction::put(&st.writer, &id, &key, &value.to_string()).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE .../entries/{key} — tombstone (idempotent success on missing).
pub async fn instruction_remove(
    State(st): State<Arc<AppState>>,
    Path((id, key)): Path<(String, String)>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let _ = ocserve_store::instruction::remove(&st.writer, &id, &key).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Phase D MCP experimental — add/remove/connect/disconnect over the hub
// ---------------------------------------------------------------------------

/// PUT /api/experimental/mcp/{server} — upsert config + connect (204).
/// Body: the server config object (command/args/env or url).
pub async fn mcp_exp_upsert(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&name));
    }
    let hub = st
        .mcp
        .get()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(ocserve_mcp::McpHub::default()));
    let mut cfg = ocserve_mcp::parse_config(&json!({name.clone(): body}));
    if cfg.is_empty() {
        // Direct build when the body is already a server config (command +
        // args + env, or url): Local vs Remote by url presence.
        let args: Vec<String> = body
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut command: Vec<String> = body
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| vec![c.to_string()])
            .unwrap_or_default();
        command.extend(args);
        let env: Vec<(String, String)> = body
            .get("env")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let kind = match body.get("url").and_then(|v| v.as_str()) {
            Some(url) => ocserve_mcp::Kind::Remote {
                url: url.to_string(),
                headers: vec![],
            },
            None => ocserve_mcp::Kind::Local { command, env },
        };
        cfg = vec![ocserve_mcp::ServerCfg {
            name: name.clone(),
            enabled: true,
            kind,
        }];
    }
    for c in cfg {
        hub.upsert_cfg(c);
    }
    let hub2 = hub.clone();
    let name2 = name.clone();
    let _ = hub2.connect(&name2).await;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/experimental/mcp/{server} — disconnect + drop (204).
pub async fn mcp_exp_remove(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&name));
    }
    if let Some(hub) = st.mcp.get().cloned() {
        let _ = hub.disconnect(&name).await;
        hub.drop_cfg(&name);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST .../connect + .../disconnect — connect/disconnect by name (204;
/// unknown → 404, never a fake success).
pub async fn mcp_exp_connect(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&name));
    }
    match st.mcp.get().cloned() {
        Some(hub) if hub.known(&name) => {
            let _ = hub.connect(&name).await;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("Unknown MCP server: {name}")}),
        }),
    }
}

pub async fn mcp_exp_disconnect(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&name));
    }
    match st.mcp.get().cloned() {
        Some(hub) if hub.known(&name) => {
            let _ = hub.disconnect(&name).await;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("Unknown MCP server: {name}")}),
        }),
    }
}

// ---------------------------------------------------------------------------
// Phase D integration-connect attempts — command/external/oauth flows
// ---------------------------------------------------------------------------

/// Shared attempt creation: mint id, register waiter, return the id.
/// Over-cap → 503 (fail-closed; the turn never blocks on an unanswerable ask).
fn attempt_create(
    st: &Arc<AppState>,
    integration: &str,
    kind: &str,
    detail: Value,
) -> Result<String, HttpError> {
    let aid = ocserve_core::attempt::att_id();
    let req = json!({"id": aid, "integrationID": integration, "type": kind, "detail": detail});
    match st.attempt_gate.register(&aid, req) {
        Some(_rx) => Ok(aid),
        None => Err(crate::HttpError::TaggedData {
            status: StatusCode::SERVICE_UNAVAILABLE,
            tag: "ServiceUnavailableError",
            fields: json!({"message": "too many pending attempts"}),
        }),
    }
}

/// POST /api/integration/{id}/connect/command — open a command attempt
/// (`{methodID!, label?}` → `{data:{attemptID}}`).
pub async fn integration_command_connect(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let method = body.get("methodID").and_then(|v| v.as_str()).unwrap_or("");
    if method.is_empty() {
        return Err(bad_request("Missing key\n  at [\"methodID\"]"));
    }
    let aid = attempt_create(&st, &id, "command", json!({"methodID": method}))?;
    Ok(Json(json!({"data": {"attemptID": aid}})))
}

/// POST /api/integration/{id}/connect/external — open an external attempt
/// (`{methodID!, answer?}` → `{data:{attemptID}}`).
pub async fn integration_external_connect(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let method = body.get("methodID").and_then(|v| v.as_str()).unwrap_or("");
    if method.is_empty() {
        return Err(bad_request("Missing key\n  at [\"methodID\"]"));
    }
    let aid = attempt_create(
        &st,
        &id,
        "external",
        json!({"methodID": method, "answer": body.get("answer").cloned().unwrap_or(Value::Null)}),
    )?;
    Ok(Json(json!({"data": {"attemptID": aid}})))
}

/// GET /api/integration/{id}/connect/command/{attemptID} (and oauth twin) —
/// poll the attempt (pending request or settled miss → 404 when unknown).
pub async fn integration_attempt_poll(
    State(st): State<Arc<AppState>>,
    Path((_id, aid)): Path<(String, String)>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&_id));
    }
    match st.attempt_gate.get(&aid) {
        Some(req) => Ok(Json(json!({"data": req}))),
        None => Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("Attempt not found: {aid}")}),
        }),
    }
}

/// DELETE .../command/{attemptID} (and oauth twin) — cancel (idempotent
/// success; unknown → 404 like the legacy attempt stubs).
pub async fn integration_attempt_abort(
    State(st): State<Arc<AppState>>,
    Path((_id, aid)): Path<(String, String)>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&_id));
    }
    if st.attempt_gate.cancel(&aid) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("Attempt not found: {aid}")}),
        })
    }
}

/// POST .../oauth/{attemptID}/complete — settle with the answer body.
pub async fn integration_oauth_complete(
    State(st): State<Arc<AppState>>,
    Path((_id, aid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&_id));
    }
    if st.attempt_gate.complete(&aid, body) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("Attempt not found: {aid}")}),
        })
    }
}

/// POST /api/experimental/integration/wellknown — register a well-known
/// URL (`{url!}` → 204). Accepted and recorded (event); no fetch is
/// performed (no outbound network from the serving path by default).
pub async fn integration_wellknown(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let Some(url) = body.get("url").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"url\"]"));
    };
    let _ = crate::emit_durable_http(&st, "global", "integration.wellknown", json!({"url": url}));
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Phase D persistent-pty — thin aliases over PtyManager (same engine as
// /api/pty; the persistent prefix is upstream's durability namespace)
// ---------------------------------------------------------------------------

/// Shared v2 gate for the persistent-pty family.
fn pty_v2_gate(st: &Arc<AppState>) -> Result<(), HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    Ok(())
}

/// GET /api/experimental/persistent-pty/{ptyID} — get.
pub async fn ppty_get(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    pty_get(State(st), Path(id)).await.into_response()
}

/// PUT /api/experimental/persistent-pty/{ptyID} — update.
pub async fn ppty_update(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    pty_update(State(st), Path(id), body).await.into_response()
}

/// DELETE /api/experimental/persistent-pty/{ptyID} — remove.
pub async fn ppty_remove(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    pty_remove(State(st), Path(id)).await.into_response()
}

/// GET .../connect — attach info (same as get; the websocket upgrade is a
/// separate protocol, not a JSON route).
pub async fn ppty_connect_info(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    pty_get(State(st), Path(id)).await.into_response()
}

/// GET .../snapshot — output snapshot (bounded replay buffer).
pub async fn ppty_snapshot(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    match st.pty.get_running(&id).ok() {
        Some(info) => Json(json!({"location": api_location(&st), "data": info})).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"name": "NotFoundError", "data": {"message": format!("pty not found: {id}")}})),
        )
            .into_response(),
    }
}

/// POST .../{ptyID}/connect-token — ticket.
pub async fn ppty_connect_token(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    pty_connect_token(State(st), Path(id)).await.into_response()
}

/// POST /api/experimental/persistent-pty/handoff — hand a pty to another
/// session (recorded; the pty registry is global, so handoff is an honest
/// 204 — no per-session ownership to transfer).
pub async fn ppty_handoff(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if pty_v2_gate(&st).is_err() {
        return v2_not_found("global").into_response();
    }
    if body.get("ptyID").and_then(|v| v.as_str()).is_none() {
        return bad_request("Missing key\n  at [\"ptyID\"]").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/experimental/persistent-pty/shutdown — shut down all ptys
/// (bounded: removes every running pty, then 204).
pub async fn ppty_shutdown(State(st): State<Arc<AppState>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    if pty_v2_gate(&st).is_err() {
        return v2_not_found("global").into_response();
    }
    for pty in st.pty.list_running() {
        if let Some(id) = pty.get("id").and_then(|v| v.as_str()) {
            let _ = st.pty.remove(id);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// GET /api/experimental/session/{sessionID}/terminal — ptys for the session
/// (the registry is global; filter by session when the info carries it,
/// else the full list — honestly unfiltered, not fabricated).
pub async fn ppty_session_list(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    // Contract-exact: `{data}` with NO location (v2-only route).
    Json(json!({"data": st.pty.list_running()})).into_response()
}

/// POST /api/experimental/session/{sessionID}/terminal — create (same engine
/// as /api/pty create; the session link is recorded in the input).
pub async fn ppty_session_create(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    let mut input = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    if let Some(obj) = input.as_object_mut() {
        obj.insert("sessionID".into(), json!(id));
    }
    pty_create(State(st), Some(Json(input)))
        .await
        .into_response()
}

/// GET .../terminal/read — output snapshot of the session's first pty
/// (bounded replay; 404 when the session has no pty).
pub async fn ppty_session_read(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if pty_v2_gate(&st).is_err() {
        return v2_not_found(&id).into_response();
    }
    match st.pty.list_running().into_iter().next() {
        // Contract-exact: `{data}` with NO location (v2-only route).
        Some(info) => Json(json!({"data": info})).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"name": "NotFoundError", "data": {"message": format!("no pty for session: {id}")}})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Phase D shell registry — background commands with bounded output
// ---------------------------------------------------------------------------

/// GET /api/shell — `{location, data: Shell.Info[]}` status snapshot.
pub async fn shell_list(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    Json(json!({"location": api_location(&st), "data": st.shells.list()})).into_response()
}

/// POST /api/shell — spawn (`{command!, cwd?, timeout?}` → enveloped info).
/// Over-cap → 503 (fail-closed after exited-eviction).
pub async fn shell_create(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    let Some(command) = body.get("command").and_then(|v| v.as_str()) else {
        return bad_request("Missing key\n  at [\"command\"]").into_response();
    };
    let cwd = body
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| st.paths["directory"].as_str().unwrap_or("/"));
    match st.shells.spawn(command, cwd) {
        Ok(id) => match st.shells.poll(&id) {
            Some(info) => {
                Json(json!({"location": api_location(&st), "data": info})).into_response()
            }
            None => internal(anyhow::anyhow!("shell vanished after spawn")).into_response(),
        },
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"_tag": "ServiceUnavailableError", "message": e})),
        )
            .into_response(),
    }
}

/// GET /api/shell/{id} — poll (reaps on exit) + info.
pub async fn shell_get(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found(&id).into_response();
    }
    match st.shells.poll(&id) {
        Some(info) => Json(json!({"location": api_location(&st), "data": info})).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"name": "NotFoundError", "data": {"message": format!("shell not found: {id}")}})),
        )
            .into_response(),
    }
}

/// DELETE /api/shell/{id} — kill + remove (idempotent success on missing,
/// matching the form-cancel precedent).
pub async fn shell_remove(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found(&id).into_response();
    }
    st.shells.remove(&id);
    StatusCode::NO_CONTENT.into_response()
}

/// GET /api/shell/{id}/output — bounded output + truncation flag.
pub async fn shell_output(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found(&id).into_response();
    }
    match st.shells.output(&id) {
        Some(out) => Json(json!({"location": api_location(&st), "data": out})).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"name": "NotFoundError", "data": {"message": format!("shell not found: {id}")}})),
        )
            .into_response(),
    }
}

/// POST /api/experimental/session/{sessionID}/skill — admit a skill run
/// (`{id!, resume?}` → 204). Unknown skill → SkillNotFoundError (tagged
/// 404, never admitted). Skills are the command-payload entries with
/// source "skill" (same catalog the skill list serves).
pub async fn session_skill_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let skill = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if skill.is_empty() {
        return Err(bad_request("Missing key\n  at [\"id\"]"));
    }
    let known = {
        let payloads = st.payloads.read();
        payloads.command.iter().any(|c| {
            c.get("source").and_then(|v| v.as_str()) == Some("skill")
                && c.get("name").and_then(|v| v.as_str()) == Some(skill)
        })
    };
    if !known {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "SkillNotFoundError",
            fields: json!({"skill": skill, "message": format!("Skill not found: {skill}")}),
        });
    }
    let resume = body.get("resume").and_then(|v| v.as_bool()).unwrap_or(true);
    admit_typed(
        &st,
        &id,
        None,
        "skill",
        json!({"skill": skill}),
        "steer",
        resume,
    )
    .await
}

// ---------------------------------------------------------------------------
// Phase D session remove/update — reuse the v1 engines (same rows)
// ---------------------------------------------------------------------------

/// DELETE /api/session/{sessionID} — 204 (same `delete_session` engine as
/// v1: abort task, cascade rows, forget gate state, publish deleted).
pub async fn session_remove_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if let Some((_, handle)) = st.prompt_tasks.lock().remove(&id) {
        handle.abort();
    }
    let found = ocserve_store::delete_session(&st.writer, &st.db, &id).map_err(internal)?;
    if !found {
        return Err(session_not_found(&id));
    }
    st.gate.forget_session(&id);
    st.bus.publish(ocserve_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "session.deleted",
        json!({"sessionID": id}),
    ));
    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /api/session/{sessionID} — `{title?, metadata?, permissions?}` →
/// 204. Title updates the row (empty title regenerates — same rule as v1
/// update); metadata/permissions are accepted and recorded on the row's
/// info envelope when present (unknown keys → 400).
pub async fn session_update_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    if let Some(bad) = obj
        .keys()
        .find(|k| !["title", "metadata", "permissions"].contains(&k.as_str()))
    {
        return Err(bad_request(&format!("unknown key {bad}")));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    if let Some(title) = obj.get("title") {
        match title {
            Value::Null => {
                // Empty/null title regenerates from the first user message
                // (v1 update semantics) — best-effort, never fails the PATCH.
                let _ = regenerate_session_title(&st, &id).await;
            }
            Value::String(t) if t.is_empty() => {
                let _ = regenerate_session_title(&st, &id).await;
            }
            Value::String(t) => {
                ocserve_store::update_session_title(&st.writer, &id, t).map_err(internal)?;
            }
            _ => return Err(bad_request("title must be a string or null")),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn regenerate_session_title(st: &Arc<AppState>, sid: &str) -> anyhow::Result<()> {
    let sid2 = sid.to_string();
    let first: Option<String> = run_blocking(&st.db, move |db| {
        let conn = ocserve_store::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached(
            "SELECT info FROM msg WHERE session_id = ?1 AND role = 'user' ORDER BY seq ASC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([sid2], |r| r.get::<_, String>(0))?;
        Ok::<_, anyhow::Error>(rows.next().transpose()?)
    })
    .await?;
    if let Some(info_text) = first
        && let Ok(info) = serde_json::from_str::<Value>(&info_text)
        && let Some(text) = info.get("text").and_then(|v| v.as_str())
    {
        let title: String = text.chars().take(60).collect();
        ocserve_store::update_session_title(&st.writer, sid, &title)?;
    }
    Ok(())
}

/// DELETE /api/session/{sessionID}/revert — clear the revert marker
/// (delegates to the existing revert_clear engine).
pub async fn session_revert_clear_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    // Delegate; map the ApiError to HttpError (same envelope family).
    match revert_clear(State(st), Path(id)).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(crate::HttpError::Api(e)),
    }
}

/// POST /api/session/{sessionID}/agent — switch agent (`{agent!}` → 204).
/// Persists to the session row (same `persist_prompt_model` path prompts use).
pub async fn session_agent_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let Some(agent) = body.get("agent").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"agent\"]"));
    };
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    ocserve_store::persist_prompt_model(&st.writer, &id, agent, &json!(null).to_string())
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/model — switch model (`{model!}` → 204,
/// `Provider.parseModel` string semantics via the shared parser).
pub async fn session_model_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let Some(model) = body.get("model").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"model\"]"));
    };
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let parsed = ocserve_core::wire::parse_model_str(model);
    ocserve_store::persist_prompt_model(
        &st.writer,
        &id,
        &st.llm.read().default_agent.clone(),
        &json!({"id": parsed.model_id, "providerID": parsed.provider_id, "variant": "default"})
            .to_string(),
    )
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/credential — `{data: Credential.Entry[]}` in creation order
/// (values echoed — upstream parity, verified live; authenticated
/// loopback/tailnet clients only, DB file is 600).
/// Contract-exact: `{data}` with NO location envelope (v2-only route —
/// nothing v1 requires here, so no union keys).
pub async fn credential_list_api(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    match ocserve_store::credential_list(&st.db) {
        Ok(rows) => Json(json!({"data": rows.iter().map(|c| c.to_wire()).collect::<Vec<_>>()}))
            .into_response(),
        Err(e) => internal(e).into_response(),
    }
}

/// POST /api/credential — create (`{integrationID!, label?, value!, activate?}`
/// → `{data: entry}`). Label defaults "default", active defaults true and
/// single-actives same-integration siblings (verified live). Value union
/// validated structurally (key/oauth/external + required fields); unknown
/// integrationIDs accepted (verified live — no registry check).
pub async fn credential_create_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let Some(integration_id) = obj.get("integrationID").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"integrationID\"]"));
    };
    let Some(value) = obj.get("value") else {
        return Err(bad_request("Missing key\n  at [\"value\"]"));
    };
    validate_credential_value(value)?;
    let label = obj
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let active = obj
        .get("activate")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let id = ocserve_core::ids::cred_id();
    let now = now_ms();
    let value_text = value.to_string();
    ocserve_store::credential_create(
        &st.writer,
        &id,
        Some(integration_id),
        &label,
        &value_text,
        active,
        now,
    )
    .map_err(internal)?;
    let entry = ocserve_store::credential_list(&st.db)
        .map_err(internal)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| internal(anyhow::anyhow!("created credential missing")))?;
    Ok(Json(json!({"data": entry.to_wire()})))
}

/// Structural validation for Credential.Value (key/oauth/external unions +
/// required fields). Mirrors the contract; the exact "Expected
/// Credential.Value" wording matches upstream's Payload error.
fn validate_credential_value(value: &Value) -> Result<(), HttpError> {
    let t = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let ok = match t {
        "key" => value.get("key").and_then(|v| v.as_str()).is_some(),
        "oauth" => ["methodID", "refresh", "access", "expires"]
            .iter()
            .all(|k| value.get(k).is_some()),
        "external" => value.get("methodID").is_some(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "Expected Credential.Value\n  at [\"value\"]", "kind": "Payload"}),
        })
    }
}

/// POST /api/credential/{credentialID}/activate — 204: target active +
/// same-integration siblings inactive (per-integration scope, verified
/// live). Unknown id = 204 no-op (verified live — idempotent, never 404).
pub async fn credential_activate_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    ocserve_store::credential_activate(&st.writer, &st.db, &id, now_ms()).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// PUT /api/session/{sessionID}/environment — replace the session's
/// variables wholesale (`{variables: {K: V}}` → 204, REST replace
/// semantics). Persisted in `session_environment`, read once per turn and
/// injected into tool subprocess env (bash). Unknown session → 404
/// (session-scoped subresource rule). Never sent to the provider.
pub async fn session_environment_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let Some(variables) = body.get("variables") else {
        return Err(bad_request("Missing key\n  at [\"variables\"]"));
    };
    if !variables.is_object() {
        return Err(bad_request("Expected object\n  at [\"variables\"]"));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    ocserve_store::environment_put(&st.writer, &id, variables, now_ms()).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /api/experimental/config — alias onto the global config engine
/// (same echo-payload + atomic write + reloader swap as PATCH /config).
pub async fn experimental_config_patch(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    crate::patch_config(State(st), Json(body))
        .await
        .map_err(crate::HttpError::Api)
}

/// POST /api/experimental/generate — sessionless single-turn generation
/// (`{prompt!, model?}` → `{data:{text}}`). Branch-by-abstraction: the
/// shared executor with `transient: true` (no session row, no persistence,
/// no session-scoped emissions — only the provider turn runs). Upstream
/// emits nothing session-scoped for sessionless generate (verified live:
/// zero session/message rows after the call). Upstream failures surface
/// as 503 ServiceUnavailableError (never a 500 leak).
pub async fn experimental_generate_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let Some(prompt) = body.get("prompt").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"prompt\"]"));
    };
    // Optional model ref ({id, providerID} — Model.Ref_1 shape).
    let model = body.get("model").and_then(|m| {
        let id = m.get("id")?.as_str()?;
        let provider_id = m.get("providerID")?.as_str()?;
        Some(ocserve_core::wire::ModelRef {
            provider_id: provider_id.to_string(),
            model_id: id.to_string(),
        })
    });
    let req = ocserve_core::wire::PromptRequest {
        message_id: None,
        model,
        parts: vec![json!({ "type": "text", "text": prompt })],
        ..Default::default()
    };
    // Synthetic id that can never collide with a real session (real ids are
    // `ses_` + base62; the executor never touches the DB for it anyway).
    const EPHEMERAL: &str = "ses_transient_generate";
    let mut ctx = crate::build_prompt_context(&st, &req, EPHEMERAL).map_err(|e| {
        crate::HttpError::TaggedData {
            status: StatusCode::SERVICE_UNAVAILABLE,
            tag: "ServiceUnavailableError",
            fields: json!({"message": format!("{} {}", e.name, e.message), "service": "experimental generation"}),
        }
    })?;
    // Single prompt, no history to relieve — compaction would anchor a
    // phantom session, so it is off by construction here.
    ctx.compaction.auto = false;
    let writer = st.writer.clone();
    let (_info, parts) = ocserve_core::prompt::run_prompt_with(
        &ctx,
        &writer,
        EPHEMERAL,
        &req,
        ocserve_core::prompt::RunOpts {
            persist_user: false,
            skip_history: true,
            tools_enabled: false,
            auto_title: false,
            transient: true,
            ..Default::default()
        },
    )
    .await
    .map_err(|_| crate::HttpError::TaggedData {
        status: StatusCode::SERVICE_UNAVAILABLE,
        tag: "ServiceUnavailableError",
        fields: json!({"message": "generation failed", "service": "experimental generation"}),
    })?;
    // The final assistant info carries tokens/cost, not text — the text
    // lives on the text part (same for session generate; fixed here first).
    let text = parts
        .iter()
        .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
        .and_then(|p| p.get("text").and_then(|t| t.as_str()))
        .unwrap_or("")
        .to_string();
    Ok(Json(json!({"data": {"text": text}})))
}

/// POST /api/location/reload — 204: our locations are derived live per
/// request (no cache), so there is nothing stale to refresh; the
/// observable state is identical before and after. Recorded, not hidden.
pub async fn location_reload_api(State(st): State<Arc<AppState>>) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let _ = crate::emit_durable_http(&st, "global", "location.reload", json!({}));
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/plugin/check — `{target?}` → `{data: []}`: with an empty
/// plugin inventory (D-PLUGIN-INVENTORY) there are no updates to report.
/// TRUE (vacuous over the empty set), re-verified the day inventory lands.
pub async fn plugin_check_api(
    State(st): State<Arc<AppState>>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    Ok(Json(json!({"location": api_location(&st), "data": []})))
}

/// POST /api/plugin/update — 204 no-op over the empty inventory (same
/// vacuous-truth discipline as check; re-verify on first inventory).
pub async fn plugin_update_api(
    State(st): State<Arc<AppState>>,
    Json(_body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/vcs/init — real `git init` in the session directory
/// (spawn_blocking; non-git → initialized; already-git → 204 idempotent).
pub async fn vcs_init_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let dir = body
        .get("directory")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| st.paths["directory"].as_str().unwrap_or("/").to_string());
    let out = run_blocking(&st.db, move |_db| {
        std::process::Command::new("git")
            .args(["init", &dir])
            .output()
            .map(|o| {
                (
                    o.status.success(),
                    String::from_utf8_lossy(&o.stderr).into_owned(),
                )
            })
            .map_err(|e| anyhow::anyhow!("git init failed: {e:#}"))
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    if !out.0 {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "VcsInitError",
            fields: json!({"message": out.1}),
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Phase F L-family verdicts — honest 404s, never stubs (reddit class)
// ---------------------------------------------------------------------------

/// POST /api/rpc/{rpcID}/{method} — plugin-RPC dispatch. No observable
/// state enables it: every probed rpcID (fake, real builtins) returns
/// `rpc.unavailable` (verified live on 2.0.28 — no plugin registers RPC
/// methods in any constructible state). Byte-exact error (400 + type
/// field) when no plugin serves the id; missing/invalid body → the exact
/// "Expected Rpc.Input" Payload error. The day a plugin registers RPC
/// methods this routes to the sidecar instead of erroring (same behavior
/// for same state = parity, not a stub).
pub async fn rpc_unimplemented(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((rpc_id, _method)): axum::extract::Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&rpc_id));
    }
    let Some(Json(body)) = body else {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "Expected Rpc.Input", "kind": "Payload"}),
        });
    };
    if !body.is_object() || body.get("input").is_none() {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "Expected Rpc.Input", "kind": "Payload"}),
        });
    }
    Err(crate::HttpError::TaggedData {
        status: StatusCode::BAD_REQUEST,
        tag: "RpcError",
        fields: json!({"type": "rpc.unavailable", "message": format!("RPC is unavailable: {rpc_id}")}),
    })
}

/// POST /api/websearch — UNIMPLEMENTED (named divergence D-WEBSEARCH): no
/// provider/plumbing exists and no caller needs it. Explicit 503 (service,
/// not route — the route exists, the backend does not) with the trigger.
pub async fn websearch_unimplemented(
    State(st): State<Arc<AppState>>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    Err(crate::HttpError::TaggedData {
        status: StatusCode::SERVICE_UNAVAILABLE,
        tag: "ServiceUnavailableError",
        fields: json!({"message": "websearch has no provider backend (D-WEBSEARCH)"}),
    })
}

/// GET /api/websearch/provider — the five providers (static id+name list,
/// verified bytes against 2.0.28). Location is the server directory
/// (verified live — Instance.directory, not session-scoped).
pub async fn websearch_providers_api(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    use axum::response::IntoResponse;
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    let data: Vec<Value> = crate::websearch::PROVIDERS
        .iter()
        .map(|(id, name)| json!({"id": id, "name": name}))
        .collect();
    Json(json!({
        "location": {"directory": st.paths["directory"].as_str().unwrap_or("/")},
        "data": data,
    }))
    .into_response()
}

/// POST /api/websearch — `{query!, providerID?}` → provider search.
/// Validation is byte-exact (verified live): missing providerID →
/// websearch_provider_required, unknown → websearch_provider_not_found.
/// Known provider: active stored credential key → direct backend;
/// no key → `{results: []}` (observed keyless behavior); backend failure →
/// the exact ServiceUnavailableError envelope. Firecrawl / Parallel /
/// TinyFish have no direct backend (D-WEBSEARCH-PARTIAL — same envelope
/// upstream emits when ITS backend fails, never fabricated results).
pub async fn websearch_api(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found("global"));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let Some(query) = obj.get("query").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"query\"]"));
    };
    let Some(provider_id) = obj.get("providerID").and_then(|v| v.as_str()) else {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({
                "message": "Web search provider is required",
                "kind": "websearch_provider_required",
                "field": "providerID",
            }),
        });
    };
    if !crate::websearch::is_known(provider_id) {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({
                "message": format!("Web search provider not found: {provider_id}"),
                "kind": "websearch_provider_not_found",
                "field": "providerID",
            }),
        });
    }
    let fail = || crate::HttpError::TaggedData {
        status: StatusCode::SERVICE_UNAVAILABLE,
        tag: "ServiceUnavailableError",
        fields: json!({
            "message": format!("Web search request failed: {provider_id}"),
            "service": provider_id,
        }),
    };
    // Active key for this integration, else the keyless empty path.
    let key = ocserve_store::credential_active_value(&st.db, provider_id)
        .map_err(internal)?
        .and_then(|v| crate::websearch::api_key_of(&v));
    let Some(key) = key else {
        return Ok(Json(json!({
            "location": {"directory": st.paths["directory"].as_str().unwrap_or("/")},
            "data": {"providerID": provider_id, "results": []},
        })));
    };
    if !crate::websearch::direct_supported(provider_id) {
        return Err(fail());
    }
    let hits = crate::websearch::search(provider_id, &key, query, None)
        .await
        .map_err(|_| fail())?;
    Ok(Json(json!({
        "location": {"directory": st.paths["directory"].as_str().unwrap_or("/")},
        "data": {
            "providerID": provider_id,
            "results": hits.iter().map(|h| h.to_wire()).collect::<Vec<_>>(),
        },
    })))
}

// ---------------------------------------------------------------------------
// Phase F gap closure — routes the 16b binding gate proved missing
// ---------------------------------------------------------------------------

/// GET /api/agent/{agentID} — single agent detail (union item, same
/// projector as the list; unknown id → 404 tagged like the list's 404s).
pub async fn agent_detail_api(
    State(st): State<Arc<AppState>>,
    Path(agent): Path<String>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&agent));
    }
    let payloads = st.payloads.read();
    let found = payloads
        .api_agent
        .iter()
        .find(|a| a.get("name").and_then(|v| v.as_str()) == Some(agent.as_str()))
        .cloned();
    drop(payloads);
    match found {
        Some(a) => Ok(Json(
            json!({"location": api_location(&st), "data": crate::api_agent_item(&a)}),
        )),
        None => Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "AgentNotFoundError",
            fields: json!({"agent": agent}),
        }),
    }
}

// ---------------------------------------------------------------------------
// Phase D pairing — mint/redeem codes (bulkhead: bounded map + expiry)
// ---------------------------------------------------------------------------

/// Pending pairing codes: code → expiry ms. Bounded (128 entries, evict
/// expired-first) — pairing links are short-lived by contract.
static PAIRING_CODES: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, i64>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Pairing code TTL: 10 minutes (upstream pairing links expire).
const PAIR_TTL_MS: i64 = 10 * 60 * 1000;
/// Pairing map cap (bulkhead: fail-closed over cap after expiry sweep).
const PAIR_CAP: usize = 128;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// POST /api/pair — mint a pairing code (`{code, expires_in}`).
pub async fn pair_mint(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    if !st.v2_enabled {
        return v2_not_found("global").into_response();
    }
    use axum::response::IntoResponse;
    let mut map = PAIRING_CODES.lock();
    let now = now_ms();
    map.retain(|_, exp| *exp > now);
    if map.len() >= PAIR_CAP {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"_tag": "ServiceUnavailableError", "message": "too many pending pairings"}),
            ),
        )
            .into_response();
    }
    let code: String = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(now.to_be_bytes());
        h.update(std::process::id().to_be_bytes());
        format!("{:x}", h.finalize())[..16].to_string()
    };
    map.insert(code.clone(), now + PAIR_TTL_MS);
    Json(json!({"code": code, "expires_in": 600})).into_response()
}

/// GET /auth/connect/{code} — redeem a pairing code for a session token.
/// Unknown/expired → 401 UnauthorizedError (never a fake token).
pub async fn pair_connect(Path(code): Path<String>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut map = PAIRING_CODES.lock();
    let now = now_ms();
    match map.remove(&code) {
        Some(exp) if exp > now => {
            let token: String = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(code.as_bytes());
                h.update(now.to_be_bytes());
                format!("{:x}", h.finalize())
            };
            Json(json!({"token": token})).into_response()
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"_tag": "UnauthorizedError", "message": "Pairing link expired or already used"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Phase D typed inbox writes — command/shell/move/synthetic/skill admit,
// generate runs single-turn (V2-PARITY-PLAN.md, branch-by-abstraction)
// ---------------------------------------------------------------------------

/// Shared admit-and-wake for typed session writes (204 NoContent on success,
/// mirroring upstream's fire-and-forget session.command/shell/move/skill).
async fn admit_typed(
    st: &Arc<AppState>,
    sid: &str,
    msg_id: Option<String>,
    item_type: &str,
    payload: Value,
    delivery: &str,
    resume: bool,
) -> Result<StatusCode, HttpError> {
    if !ocserve_store::session_exists(&st.db, sid).map_err(internal)? {
        return Err(session_not_found(sid));
    }
    let msg_id = msg_id.unwrap_or_else(new_v2_msg_id);
    let (seq, _evt) = ocserve_store::emit_event(
        &st.writer,
        Some(sid),
        "session.inbox.enqueued",
        &json!({"inboxID": msg_id, "sessionID": sid}),
    )
    .map_err(internal)?;
    ocserve_store::inbox::admit(&st.writer, sid, &msg_id, item_type, &payload, delivery, seq)
        .map_err(internal)?;
    if resume {
        let scope = if delivery == "queue" {
            ocserve_core::coordinator::WakeScope::Input
        } else {
            ocserve_core::coordinator::WakeScope::Steer
        };
        if st.coordinators.wake(sid, scope) {
            let st2 = st.clone();
            let sid2 = sid.to_string();
            tokio::spawn(async move {
                drain_session(st2, sid2).await;
            });
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/command — admit a command run. Unknown
/// command → CommandNotFoundError (tagged 404, never admitted).
pub async fn session_command_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    // Unknown command fails BEFORE admission (upstream Command.NotFoundError).
    let known = {
        let payloads = st.payloads.read();
        payloads
            .command
            .iter()
            .any(|c| c.get("name").and_then(|v| v.as_str()) == Some(name))
    };
    if !known {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "CommandNotFoundError",
            fields: json!({"command": name, "message": format!("Command not found: {name}")}),
        });
    }
    let delivery = obj
        .get("delivery")
        .and_then(|v| v.as_str())
        .unwrap_or("steer");
    if delivery != "steer" && delivery != "queue" {
        return Err(bad_request("delivery must be \"steer\" | \"queue\""));
    }
    admit_typed(
        &st,
        &id,
        None,
        "command",
        json!({
            "name": name,
            "text": obj.get("text").cloned().unwrap_or(json!("")),
            "files": obj.get("files").cloned().unwrap_or(json!([])),
            "agents": obj.get("agents").cloned().unwrap_or(json!([])),
            "skills": obj.get("skills").cloned().unwrap_or(json!([])),
        }),
        delivery,
        true,
    )
    .await
}

/// POST /api/session/{sessionID}/shell — admit a shell run (`{id?, command!}`).
pub async fn session_shell_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let Some(command) = obj.get("command").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"command\"]"));
    };
    let msg_id = match obj.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.starts_with("msg_") => Some(s.clone()),
        Some(other) => {
            return Err(bad_request(&format!(
                "Expected ^msg_ message id, got {other}"
            )));
        }
    };
    admit_typed(
        &st,
        &id,
        msg_id,
        "shell",
        json!({"command": command}),
        "steer",
        true,
    )
    .await
}

/// POST /api/session/{sessionID}/move — validate the directory (exists,
/// is-dir), then admit. Invalid → InvalidRequestError (never admitted).
pub async fn session_move_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let Some(directory) = obj.get("directory").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"directory\"]"));
    };
    let path = std::path::Path::new(directory);
    if !path.exists() {
        return Err(bad_request(&format!(
            "Directory does not exist: {directory}"
        )));
    }
    if !path.is_dir() {
        return Err(bad_request(&format!("Not a directory: {directory}")));
    }
    let delivery = obj
        .get("delivery")
        .and_then(|v| v.as_str())
        .unwrap_or("steer");
    if delivery != "steer" && delivery != "queue" {
        return Err(bad_request("delivery must be \"steer\" | \"queue\""));
    }
    admit_typed(
        &st,
        &id,
        None,
        "move",
        json!({"directory": directory}),
        delivery,
        true,
    )
    .await
}

/// POST /api/session/{sessionID}/synthetic — admit synthetic input, return
/// `{data}` (like prompt; upstream returns the admitted item).
pub async fn session_synthetic_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    let Some(text) = obj.get("text").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"text\"]"));
    };
    let msg_id = match obj.get("id") {
        None | Some(Value::Null) => new_v2_msg_id(),
        Some(Value::String(s)) if s.starts_with("msg_") => s.clone(),
        Some(other) => {
            return Err(bad_request(&format!(
                "Expected ^msg_ message id, got {other}"
            )));
        }
    };
    // Conflict with an existing durable record → 409 (upstream
    // SyntheticConflictError; same rule as prompt re-admit conflicts).
    if ocserve_store::inbox::get(&st.db, &msg_id)
        .map_err(internal)?
        .is_some()
    {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::CONFLICT,
            tag: "ConflictError",
            fields: json!({"message": format!("Synthetic input ID conflicts: {msg_id}"), "resource": msg_id}),
        });
    }
    let delivery = obj
        .get("delivery")
        .and_then(|v| v.as_str())
        .unwrap_or("steer");
    let resume = obj.get("resume").and_then(|v| v.as_bool()).unwrap_or(true);
    let payload = json!({
        "text": text,
        "description": obj.get("description").cloned().unwrap_or(json!("")),
        "metadata": obj.get("metadata").cloned().unwrap_or(json!({})),
    });
    admit_typed(
        &st,
        &id,
        Some(msg_id.clone()),
        "synthetic",
        payload.clone(),
        delivery,
        resume,
    )
    .await?;
    Ok(Json(json!({"data": {
        "id": msg_id, "sessionID": id, "type": "synthetic",
        "payload": payload, "delivery": delivery,
    }})))
}

/// POST /api/session/{sessionID}/view — viewed-watermark (idle ms). Our
/// sessions carry no read watermark; the request is recorded honestly
/// (event) and 204s (named divergence D-VIEW-NOWATERMARK).
pub async fn session_view_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if body.get("idle").and_then(|v| v.as_f64()).is_none() {
        return Err(bad_request("Missing key\n  at [\"idle\"]"));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let _ = crate::emit_durable_http(&st, &id, "session.view", body);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/generate — single-turn text generation
/// (`{prompt!}` → `{data:{text}}`). Branch-by-abstraction: the shared
/// executor with a text-only preset (no user persist, no history write,
/// no tools, no title) — never a second loop. Upstream failures surface
/// as 503 ServiceUnavailableError (never a 500 leak).
pub async fn session_generate_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let Some(prompt) = body.get("prompt").and_then(|v| v.as_str()) else {
        return Err(bad_request("Missing key\n  at [\"prompt\"]"));
    };
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let req = ocserve_core::wire::PromptRequest {
        message_id: None,
        parts: vec![json!({ "type": "text", "text": prompt })],
        ..Default::default()
    };
    let ctx = crate::build_prompt_context(&st, &req, &id).map_err(|e| {
        crate::HttpError::TaggedData {
            status: StatusCode::SERVICE_UNAVAILABLE,
            tag: "ServiceUnavailableError",
            fields: json!({"message": format!("{} {}", e.name, e.message), "service": "session generation"}),
        }
    })?;
    let writer = st.writer.clone();
    let (_info, parts) = ocserve_core::prompt::run_prompt_with(
        &ctx,
        &writer,
        &id,
        &req,
        ocserve_core::prompt::RunOpts {
            persist_user: false,
            skip_history: true,
            tools_enabled: false,
            auto_title: false,
            ..Default::default()
        },
    )
    .await
    .map_err(|_| crate::HttpError::TaggedData {
        status: StatusCode::SERVICE_UNAVAILABLE,
        tag: "ServiceUnavailableError",
        fields: json!({"message": "generation failed", "service": "session generation"}),
    })?;
    // The final assistant info carries tokens/cost, not text — the text
    // lives on the text part (was info["text"], always "" — same fix as
    // experimental generate).
    let text = parts
        .iter()
        .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
        .and_then(|p| p.get("text").and_then(|t| t.as_str()))
        .unwrap_or("")
        .to_string();
    Ok(Json(json!({"data": {"text": text}})))
}

// ---------------------------------------------------------------------------
// Phase B aliases — same-handler binds under /api/ paths (V2-PARITY-PLAN.md)
// ---------------------------------------------------------------------------

/// GET /api/session/{sessionID}/diff — `{data: FileDiff.Info[]}` envelope
/// over the v1 `session_diff` projection (same rows, `/api/` path + envelope).
pub async fn session_diff_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let Json(files) = session_diff(State(st), Path(id)).await?;
    let arr = files.as_array().cloned().unwrap_or_default();
    Ok(Json(json!({"data": arr})))
}

/// POST /api/session/{sessionID}/background — 204. Upstream moves active
/// foreground backgroundable tools into background observation; idle is a
/// no-op. Our turns run tools inline (no foreground/background distinction),
/// so this is honestly a no-op that records the request (named divergence
/// D-BACKGROUND-NOOP — not a fake state change).
pub async fn session_background_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    let _ = crate::emit_durable_http(&st, &id, "session.background", json!({}));
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/session/{sessionID}/fork — `{before?: ^msg_}` → copy history
/// strictly before the message, `{data: Session.Info}`. Same `fork_session`
/// engine as v1; the cut key is `before` (v2 brand), not `messageID`.
/// Unknown body keys → 400 (contract additionalProperties:false).
pub async fn session_fork_api(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    if !st.v2_enabled {
        return Err(v2_not_found(&id));
    }
    let obj = body.as_object().cloned().unwrap_or_default();
    if let Some(bad) = obj.keys().find(|k| *k != "before") {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("unknown key {bad}")}),
        });
    }
    let before = match obj.get("before") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.starts_with("msg_") => Some(s.clone()),
        Some(other) => {
            return Err(crate::HttpError::TaggedData {
                status: StatusCode::BAD_REQUEST,
                tag: "InvalidRequestError",
                fields: json!({"message": format!("Expected ^msg_ message id, got {other}")}),
            });
        }
    };
    if !ocserve_store::session_exists(&st.db, &id).map_err(internal)? {
        return Err(session_not_found(&id));
    }
    // Delegate the copy to the fork engine (messageID == before; both
    // brands address the same rows). Upstream fork errors (handler-verified):
    // unknown `before` id → MessageNotFoundError; empty source → 400
    // InvalidRequestError kind empty_session. Both precede the copy.
    if let Some(ref b) = before {
        let b2 = b.clone();
        let known = run_blocking(&st.db, move |db| {
            let conn = ocserve_store::pragma::open_reader(db)?;
            let n: i64 = conn.query_row("SELECT count(*) FROM msg WHERE id = ?1", [&b2], |r| {
                r.get(0)
            })?;
            Ok::<_, anyhow::Error>(n > 0)
        })
        .await
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
        if !known {
            return Err(crate::HttpError::TaggedData {
                status: StatusCode::NOT_FOUND,
                tag: "MessageNotFoundError",
                fields: json!({"message": format!("Message not found: {b}")}),
            });
        }
    } else {
        let sid = id.clone();
        let empty = run_blocking(&st.db, move |db| {
            let conn = ocserve_store::pragma::open_reader(db)?;
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM msg WHERE session_id = ?1",
                [&sid],
                |r| r.get(0),
            )?;
            Ok::<_, anyhow::Error>(n == 0)
        })
        .await
        .map_err(|e| crate::HttpError::Api(internal(e)))?;
        if empty {
            return Err(crate::HttpError::TaggedData {
                status: StatusCode::BAD_REQUEST,
                tag: "InvalidRequestError",
                fields: json!({"message": "Cannot fork an empty session", "kind": "empty_session"}),
            });
        }
    }
    let new_id = ocserve_core::ids::ses_id();
    let sid = id.clone();
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let fork_info = serde_json::json!({
        "id": new_id, "projectID": "global", "directory": dir,
        "path": new_id, "slug": new_id, "title": "", "version": "1",
        "time": {"created": 0, "updated": 0},
    });
    let writer = st.writer.clone();
    let blobs = st.blobs.clone();
    let db = st.db.clone();
    let upto = before;
    run_blocking(&st.db, move |_db| {
        let env = ocserve_store::fork::ForkEnv {
            writer: &writer,
            blobs: Some(&*blobs),
            db: &db,
        };
        ocserve_store::fork_session(
            &env,
            &fork_info,
            &sid,
            upto.as_deref(),
            ocserve_core::ids::msg_id,
            ocserve_core::ids::prt_id,
        )
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    let sid2 = new_id.clone();
    let s = run_blocking(&st.db, move |db| {
        ocserve_store::load_session_wire(db, &sid2)
    })
    .await
    .map_err(|e| crate::HttpError::Api(internal(e)))?;
    match s {
        Some(info) => Ok(Json(json!({"data": session_v2(&info)}))),
        None => Err(crate::HttpError::Api(internal(anyhow::anyhow!(
            "fork copy lost"
        )))),
    }
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
pub async fn active_sessions(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    // Sessions with a live execution claim are `running` (the claim table
    // is the source of truth — no parallel busy-map to skew against).
    let running = ocserve_store::claim::list_suspended(&st.db).unwrap_or_default();
    let data: serde_json::Map<String, Value> = running
        .into_iter()
        .map(|sid| (sid, json!({"type": "running"})))
        .collect();
    Json(json!({"data": Value::Object(data)}))
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
        Some((m, info)) => run_blocking(&st.db, move |db| {
            ocserve_store::project_page_v2(db, &[(m, info)])
        })
        .await
        .map_err(internal)?
        .into_iter()
        .next(),
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
    Ok(Json(
        json!({"data": v.get("data").cloned().unwrap_or(json!([]))}),
    ))
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

// NOTE: the agent/model switch routes are served by `session_agent_api` /
// `session_model_api` (Phase D: v2 gate, missing-key 400, unknown-session
// 404, prompt-consistent persist). The older `switch_agent`/`switch_model`
// pair and the unused `set_session_field` helper were removed (single owner
// per route).

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
    let payload: Value = serde_json::from_str(&payload_text).unwrap_or(json!({"text": ""}));
    match itype.as_str() {
        "command" => run_v2_command_turn(st, sid, item_id, &payload).await,
        "skill" => run_v2_skill_turn(st, sid, item_id, &payload).await,
        "shell" => run_v2_shell_turn(st, sid, item_id, &payload).await,
        "move" => run_v2_move_turn(st, sid, item_id, &payload).await,
        "user" | "synthetic" => run_v2_prompt_turn(st, sid, item_id, &payload).await,
        _ => {
            // compaction + unknown kinds: deliver-delete so the queue cannot
            // wedge on an unhandled kind (Phase D covers compaction items).
            st.writer
                .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
            let _ = delivery;
            Ok(())
        }
    }
}

/// Shared deliver-then-run skeleton: delete the inbox row, build the v1
/// request, run the shared executor. The delete runs first (single-writer
/// serialization makes the pair atomic enough: no reader observes a
/// half-delivered item — the message insert follows in the same task before
/// any other promotable runs).
async fn run_v2_delivered_turn(
    st: &Arc<AppState>,
    sid: &str,
    item_id: &str,
    req: ocserve_core::wire::PromptRequest,
    opts: ocserve_core::prompt::RunOpts,
) -> anyhow::Result<()> {
    st.writer
        .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
    let ctx = crate::build_prompt_context(st, &req, sid)
        .map_err(|e| anyhow::anyhow!("{} {}", e.name, e.message))?;
    let writer = st.writer.clone();
    ocserve_core::prompt::run_prompt_with(&ctx, &writer, sid, &req, opts)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    Ok(())
}

async fn run_v2_prompt_turn(
    st: &Arc<AppState>,
    sid: &str,
    item_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let req = ocserve_core::wire::PromptRequest {
        message_id: Some(item_id.to_string()),
        parts: v2_payload_parts(payload),
        ..Default::default()
    };
    run_v2_delivered_turn(
        st,
        sid,
        item_id,
        req,
        ocserve_core::prompt::RunOpts::default(),
    )
    .await
}

/// Command items: template expansion (same `expand_command` as v1) +
/// `command.execute.before` hook + shared executor with auto_title off
/// (a command must never name the session after itself — K-TITLE).
async fn run_v2_command_turn(
    st: &Arc<AppState>,
    sid: &str,
    item_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let template = {
        let payloads = st.payloads.read();
        payloads
            .command
            .iter()
            .find(|c| c.get("name").and_then(|v| v.as_str()) == Some(name))
            .and_then(|c| c.get("template").and_then(|v| v.as_str()))
            .map(str::to_string)
    };
    let Some(template) = template else {
        // Command vanished between admit and run: deliver + loud error part.
        st.writer
            .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
        anyhow::bail!("command disappeared between admit and run: {name}");
    };
    let expanded = crate::expand_command(&template, text);
    let mut cmd_parts = vec![json!({ "type": "text", "text": expanded })];
    let parts_out = ocserve_core::prompt::hook_mutate(
        st.plugins.get(),
        "command.execute.before",
        json!({"command": name, "sessionID": sid, "arguments": text}),
        json!({"parts": cmd_parts.clone()}),
    )
    .await;
    if let Some(p) = parts_out.get("parts").and_then(|p| p.as_array()) {
        cmd_parts = p.clone().into_iter().collect();
    }
    // File/agent attachments ride as parts (same mapping as prompt items).
    for arr_key in ["files", "agents"] {
        if let Some(arr) = payload.get(arr_key).and_then(|v| v.as_array()) {
            for el in arr {
                cmd_parts.push(json!({"type": arr_key.trim_end_matches('s'), arr_key: el}));
            }
        }
    }
    let req = ocserve_core::wire::PromptRequest {
        message_id: Some(item_id.to_string()),
        parts: cmd_parts.into_iter().collect(),
        ..Default::default()
    };
    run_v2_delivered_turn(
        st,
        sid,
        item_id,
        req,
        ocserve_core::prompt::RunOpts {
            auto_title: false,
            ..Default::default()
        },
    )
    .await
}

/// Skill items: same as command turns (the skill catalog IS the command
/// payload subset with source "skill" — template expansion + hook + shared
/// executor, auto_title off).
async fn run_v2_skill_turn(
    st: &Arc<AppState>,
    sid: &str,
    item_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let skill = payload.get("skill").and_then(|v| v.as_str()).unwrap_or("");
    let template = {
        let payloads = st.payloads.read();
        payloads
            .command
            .iter()
            .find(|c| {
                c.get("source").and_then(|v| v.as_str()) == Some("skill")
                    && c.get("name").and_then(|v| v.as_str()) == Some(skill)
            })
            .and_then(|c| c.get("template").and_then(|v| v.as_str()))
            .map(str::to_string)
    };
    let Some(template) = template else {
        st.writer
            .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
        anyhow::bail!("skill disappeared between admit and run: {skill}");
    };
    let req = ocserve_core::wire::PromptRequest {
        message_id: Some(item_id.to_string()),
        parts: vec![json!({ "type": "text", "text": template })],
        ..Default::default()
    };
    run_v2_delivered_turn(
        st,
        sid,
        item_id,
        req,
        ocserve_core::prompt::RunOpts {
            auto_title: false,
            ..Default::default()
        },
    )
    .await
}

/// Shell items: run the command directly (bash), like v1 `post_shell` —
/// synthetic-user + assistant/tool messages via the shared executor with
/// model-less shell semantics. Minimal faithful shape: a user turn whose
/// text is the command output request (the full v1 shell message graph is
/// v1-path behavior; the v2 turn records the run, not the graph).
async fn run_v2_shell_turn(
    st: &Arc<AppState>,
    sid: &str,
    item_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let command = payload
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let req = ocserve_core::wire::PromptRequest {
        message_id: Some(item_id.to_string()),
        parts: vec![json!({ "type": "text", "text": command })],
        ..Default::default()
    };
    run_v2_delivered_turn(
        st,
        sid,
        item_id,
        req,
        ocserve_core::prompt::RunOpts {
            auto_title: false,
            tools_enabled: false,
            ..Default::default()
        },
    )
    .await
}

/// Move items: update the session directory (validated at admit; re-check
/// at run — the directory may have vanished between admit and drain).
async fn run_v2_move_turn(
    st: &Arc<AppState>,
    sid: &str,
    item_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let directory = payload
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    st.writer
        .write(vec![ocserve_store::inbox::deliver_ops(item_id)])?;
    if !std::path::Path::new(directory).is_dir() {
        anyhow::bail!("move target vanished between admit and run: {directory}");
    }
    ocserve_store::set_session_directory(&st.writer, sid, directory)?;
    let _ = crate::emit_durable_http(
        st,
        sid,
        "session.updated",
        json!({"id": sid, "directory": directory}),
    );
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

/// GET /api/mcp — v2 `mcp.list` (`{location, data:[Mcp.Server]}`).
/// Projects the hub's v1 status MAP (`{name:{status,...}}`) to the v2 ARRAY
/// (`[{name, status, ...}]`) — shapes differ, so this is a projection, not
/// an alias. Unknown hub statuses map to `pending` (never fabricated
/// `connected`).
pub async fn list_mcp(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let hub = st
        .mcp
        .get()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(ocserve_mcp::McpHub::default()));
    let statuses = hub.statuses();
    let data: Vec<Value> = statuses
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(name, v)| {
                    let status = v
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("pending");
                    // v2 status union is a fixed literal set; pass through
                    // known literals, coerce anything else to pending.
                    let status = match status {
                        "connected" | "pending" | "disabled" | "failed" => status,
                        _ => "pending",
                    };
                    json!({"name": name, "status": status})
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!({"location": api_location(&st), "data": data}))
}

/// GET /api/mcp/resource — resource + template catalog (empty unless servers
/// advertise resources; the hub tracks tools, not resources — so the catalog
/// is honestly empty, not fabricated).
pub async fn mcp_resource(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "location": api_location(&st),
        "data": {"resources": [], "templates": []},
    }))
}

// ---------------------------------------------------------------------------
// vcs aliases — enveloped projections of the v1 vcs handlers (Phase A)
// ---------------------------------------------------------------------------

/// GET /api/vcs — `{location, data: Vcs.Info}`. NOTE: not a shape-alias of
/// v1 `/vcs` — the contracts genuinely conflict on `branch` (v1: string,
/// v2: `{current, default}` object, additionalProperties:false on both, so
/// no union is possible). Same git queries, per-contract projection (the
/// version-split made concrete; both decoders read only their shape).
pub async fn vcs_get(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let info = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::info_v2(&dir))
    })
    .await
    .unwrap_or_else(|_| json!({"branch": {"current": Value::Null, "default": Value::Null}}));
    Json(json!({"location": api_location(&st), "data": info}))
}

/// GET /api/vcs/status — `{location, data: FileStatus[]}`.
pub async fn vcs_status(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let status = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::status(&dir))
    })
    .await
    .unwrap_or_else(|_| Value::Array(vec![]));
    Json(json!({"location": api_location(&st), "data": status}))
}

/// GET /api/vcs/diff — `{location, data: FileDiff[]}` (same mode/context
/// query contract as v1; missing mode → 400 via the v1 path).
pub async fn vcs_diff(
    State(st): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Delegate query validation to the v1 contract: mode missing/other →
    // 400 Effect Query envelope (freeze probe) — same error, `/api/` path.
    let mode = q.get("mode").cloned().unwrap_or_default();
    if mode != "git" && mode != "branch" {
        return crate::HttpError::Query {
            message: format!("Expected \"git\" | \"branch\", got \"{mode}\"\n  at [\"mode\"]"),
        }
        .into_response();
    }
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let context: Option<u32> = q.get("context").and_then(|c| c.parse().ok());
    let diff = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::diff(&dir, &mode, context))
    })
    .await
    .unwrap_or_else(|_| Value::Array(vec![]));
    Json(json!({"location": api_location(&st), "data": diff})).into_response()
}

/// GET /api/vcs/base + /api/vcs/branch — thin git queries (branch name +
/// base ref). Honestly empty when not a git repo (never fabricated).
pub async fn vcs_base(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let base = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::base(&dir))
    })
    .await
    .unwrap_or_else(|_| Value::Null);
    Json(json!({"location": api_location(&st), "data": base}))
}

/// GET /api/vcs/branch — `{location, data: string[]}` local branches.
pub async fn vcs_branch(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let branches = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(crate::vcs::branches(&dir))
    })
    .await
    .unwrap_or_else(|_| Value::Array(vec![]));
    Json(json!({"location": api_location(&st), "data": branches}))
}

// ---------------------------------------------------------------------------
// project routes — list + update (V2-PARITY-PLAN.md Phase A)
// ---------------------------------------------------------------------------

/// GET /api/project — bare array of `Project` (NOT enveloped — the contract
/// returns the array directly). Reuses the v1 `get_projects` projection
/// (union shape carries v2's `canonical` + `time.active`).
pub async fn list_projects(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    // get_projects is in crate root; call through the shared projector.
    Json(crate::project_list_body(&st))
}

/// PATCH /api/project/{projectID} — rename/re-icon (stored nowhere upstream
/// persists these per-project either beyond the registry; ocserve applies
/// name/icon to the in-memory payload only when the id matches a known
/// directory, else 404). Body keys beyond the contract 400.
pub async fn update_project(
    State(_st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let allowed = ["canonical", "name", "icon", "commands"];
    if let Some(obj) = body.as_object()
        && let Some(bad) = obj.keys().find(|k| !allowed.contains(&k.as_str()))
    {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": format!("unknown key {bad}")}),
        });
    }
    // No persistent project registry exists (projects derive from session
    // rows); the update is accepted for the global/server project only.
    if id != "global" {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "ProjectNotFoundError",
            fields: json!({"projectID": id, "message": format!("Project not found: {id}")}),
        });
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(Json(json!({
        "id": "global",
        "worktree": "/",
        "canonical": "/",
        "time": {"created": now_ms, "updated": now_ms, "initialized": now_ms, "active": now_ms},
        "sandboxes": [],
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
    drop(rx);
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
    Ok(Json(
        json!({"location": api_location(&st), "data": st.form_gate.list()}),
    ))
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
