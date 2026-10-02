//! refine-http: axum router, auth, byte-golden SSE, upstream error envelope.

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Freeze (PLAN §3): version reported by `/global/health`.
pub const FREEZE_VERSION: &str = "1.18.31";

/// Pre-assembled payloads for config-derived routes (built by refine-cli's
/// `Runtime::load()`; kept crate-local so http doesn't depend on cli).
#[derive(Default)]
pub struct Payloads {
    pub config: serde_json::Value,
    pub agent: Vec<serde_json::Value>,
    pub api_agent: Vec<serde_json::Value>,
    pub command: Vec<serde_json::Value>,
    pub config_providers: serde_json::Value,
    pub provider: serde_json::Value,
    pub console: serde_json::Value,
    pub capabilities: serde_json::Value,
}

/// Upstream error envelope: {"name":"NotFoundError","data":{"message":"..."}} (captured live).
pub struct ApiError {
    pub status: StatusCode,
    pub name: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            name: "NotFoundError",
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"name": self.name, "data": {"message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

/// Handler error: normal envelopes + Effect HttpApi tagged decode errors
/// (freeze parity: `?before=` → body exactly {"_tag":"BadRequest"} §1090).
pub enum HttpError {
    Api(ApiError),
    Tagged {
        status: StatusCode,
        tag: &'static str,
    },
}

impl From<ApiError> for HttpError {
    fn from(e: ApiError) -> Self {
        HttpError::Api(e)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        match self {
            HttpError::Api(e) => e.into_response(),
            HttpError::Tagged { status, tag } => {
                let body = format!("{{\"_tag\":\"{tag}\"}}");
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
                    .into_response()
            }
        }
    }
}

/// session → (generation, background prompt task) for POST /abort.
pub type PromptTask = (u64, tokio::task::JoinHandle<()>);

/// App state shared by handlers.
pub struct AppState {
    /// Static config payload served at GET /config (recorded corpus).
    pub config: serde_json::Value,
    /// Agents payload for GET /agent (default-first order).
    pub agent: Vec<serde_json::Value>,
    /// Agents for GET /api/agent (declaration order, natives first).
    pub api_agent: Vec<serde_json::Value>,
    /// Commands payload for GET /command.
    pub command: Vec<serde_json::Value>,
    /// GET /config/providers payload (providers + default).
    pub config_providers: serde_json::Value,
    /// GET /provider payload (all + default + connected).
    pub provider: serde_json::Value,
    /// GET /experimental/console payload.
    pub console: serde_json::Value,
    /// GET /experimental/capabilities payload.
    pub capabilities: serde_json::Value,
    /// Known sessions (M1: served from store as it comes online).
    pub sessions: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
    /// Runtime paths for GET /path (env-derived at boot; never stored in repo).
    pub paths: serde_json::Value,
    /// Auth mode: None = off (freeze default), Some(("user","pass")) = basic.
    pub auth: Option<(String, String)>,
    /// Request counter for /metrics (bounded, monotonic).
    pub requests: std::sync::atomic::AtomicU64,
    // ---- M2 prompt runtime (wired by CLI) ----
    /// Bounded broadcast of encoded global-event frames.
    pub bus: refine_core::EventBus,
    pub db: std::path::PathBuf,
    pub blobs: std::sync::Arc<refine_store::BlobStore>,
    pub writer: std::sync::Arc<refine_store::Writer>,
    /// Provider registry + default model + agent systems (from Runtime).
    pub llm: LlmRegistry,
    /// Permission rendezvous shared by runner + reply routes.
    pub gate: std::sync::Arc<refine_core::PermissionGate>,
    /// MCP hub probed at serve boot (OnceLock: tests run without probing).
    pub mcp: std::sync::OnceLock<std::sync::Arc<refine_mcp::McpHub>>,
    /// Plugin sidecar (M4b): async mutex — trigger() takes &mut across await.
    pub plugins: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<refine_plugin::Sidecar>>>,
    /// Per-session prompt serialization (queue semantics). Bounded: entries
    /// exist only for in-flight prompts + transient races, removed post-run
    /// when uncontended (AGENTS §2.3 — active-set bound, no eviction).
    pub prompt_locks: std::sync::Arc<
        parking_lot::Mutex<
            std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
        >,
    >,
    /// Background prompt tasks per session: (generation, JoinHandle) —
    /// POST /abort cancels the current generation (v1 session.abort).
    /// Bound: one entry per in-flight async prompt, removed on completion.
    pub prompt_tasks:
        std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<String, PromptTask>>>,
    /// Monotonic generation source for prompt_tasks.
    pub prompt_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// LLM endpoint resolution for the prompt runner (assembled by Runtime).
#[derive(Default)]
pub struct LlmRegistry {
    /// providerID → (base_url, api_key)
    pub endpoints: std::collections::HashMap<String, (String, String)>,
    /// (providerID, modelID) → (input, output, cache_read) USD/MTok
    pub pricing: std::collections::HashMap<(String, String), (f64, f64, f64)>,
    /// default (providerID, modelID) from opencode state
    pub default_model: (String, String),
    /// agent name → system prompt (only agents with explicit prompts)
    pub systems: std::collections::HashMap<String, String>,
    /// config default_agent (upstream session rows record it at prompt time)
    pub default_agent: String,
}

/// Real runtime pieces passed at construction (no interior mutability, no unsafe).
pub struct Wires {
    pub db: std::path::PathBuf,
    pub blobs: std::sync::Arc<refine_store::BlobStore>,
    pub writer: std::sync::Arc<refine_store::Writer>,
    pub llm: LlmRegistry,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Self::with_auth(None)
    }

    pub fn with_auth(auth: Option<(String, String)>) -> Arc<Self> {
        Self::with_payloads(auth, Payloads::default())
    }

    /// Build from pre-assembled route payloads with a temp-backed store
    /// (tests / simple construction — real pieces, no placeholders).
    pub fn with_payloads(auth: Option<(String, String)>, p: Payloads) -> Arc<Self> {
        let dir = std::env::temp_dir().join(format!(
            "refine-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp data dir");
        let db = refine_store::writer::db_path(&dir);
        let writer =
            std::sync::Arc::new(refine_store::Writer::spawn(db.clone()).expect("temp writer"));
        let blobs = std::sync::Arc::new(
            refine_store::BlobStore::new(dir.join("blobs")).expect("temp blobs"),
        );
        Self::with_wiring(
            auth,
            p,
            Wires {
                db,
                blobs,
                writer,
                llm: LlmRegistry::default(),
            },
        )
    }

    /// One-shot construction with real runtime wiring (CLI path).
    pub fn with_wiring(auth: Option<(String, String)>, p: Payloads, w: Wires) -> Arc<Self> {
        let home = std::env::var("HOME").unwrap_or_default();
        let worktree = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".into());
        Arc::new(Self {
            config: p.config,
            agent: p.agent,
            api_agent: p.api_agent,
            command: p.command,
            config_providers: p.config_providers,
            provider: p.provider,
            console: p.console,
            capabilities: p.capabilities,
            sessions: parking_lot::RwLock::new(HashMap::new()),
            // upstream /path shape (keys golden: home/state/config/worktree/directory)
            paths: json!({
                "home": home,
                "state": format!("{home}/.local/state/opencode"),
                "config": format!("{home}/.config/opencode"),
                "worktree": worktree,
                "directory": worktree,
            }),
            auth,
            requests: std::sync::atomic::AtomicU64::new(0),
            bus: refine_core::EventBus::new(),
            gate: refine_core::PermissionGate::new(),
            mcp: std::sync::OnceLock::new(),
            plugins: std::sync::OnceLock::new(),
            prompt_locks: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            prompt_tasks: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            prompt_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            db: w.db,
            blobs: w.blobs,
            writer: w.writer,
            llm: w.llm,
        })
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({"healthy": true, "version": FREEZE_VERSION}))
}

async fn get_config(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.config.clone())
}

async fn get_agent(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.agent.clone())
}

async fn get_command(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.command.clone())
}

async fn get_config_providers(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.config_providers.clone())
}

async fn get_provider(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.provider.clone())
}

async fn get_console(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.console.clone())
}

async fn get_capabilities(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.capabilities.clone())
}

/// v2-style location envelope shared by /api/* routes (captured live).
fn api_location(st: &AppState) -> Value {
    let dir = st.paths["directory"].as_str().unwrap_or("/");
    json!({
        "directory": dir,
        "project": {"id": "global", "directory": "/"},
    })
}

/// GET /api/location — captured shape (keys golden: directory/project/{id,directory}).
async fn get_api_location(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(api_location(&st))
}

/// GET /api/agent — v2-shaped agent list (permission triple renamed to
/// {action, resource, effect}; natives-first order; no plugin agents).
async fn get_api_agent(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let data: Vec<Value> = st
        .api_agent
        .iter()
        .map(|a| {
            let perms: Vec<Value> = a["permission"]
                .as_array()
                .map(|rules| {
                    rules
                        .iter()
                        .map(|r| {
                            json!({
                                "action": r["permission"],
                                "resource": r["pattern"],
                                "effect": r["action"],
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut out = json!({
                "id": a["name"],
                "hidden": a.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false),
                "mode": a["mode"],
                "permissions": perms,
                "request": {"headers": {}, "body": {}},
            });
            if let Some(d) = a.get("description") {
                out["description"] = d.clone();
            }
            // system: explicit prompt, else the captured build default
            if let Some(p) = a.get("prompt") {
                out["system"] = p.clone();
            } else if a["name"] == "build" {
                out["system"] = json!(BUILD_SYSTEM_BLURB);
            }
            out
        })
        .collect();
    Json(json!({"location": api_location(&st), "data": data}))
}

/// Captured from live /api/agent (build has no prompt; upstream serves this blurb).
const BUILD_SYSTEM_BLURB: &str = "You are an AI coding agent. Help the user accomplish software \
engineering tasks by inspecting the workspace, making targeted changes, and using tools according \
to the configured permissions.";

/// GET /api/command — {location, data:[{description,name,template}]}.
async fn get_api_command(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let data: Vec<Value> = st
        .command
        .iter()
        .map(|c| {
            json!({
                "name": c["name"],
                "description": c.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "template": c.get("template").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect();
    Json(json!({"location": api_location(&st), "data": data}))
}

/// GET /api/reference — always empty (no references configured).
async fn get_api_reference(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({"location": api_location(&st), "data": []}))
}

/// Empty-shape routes captured live: {}, [].
async fn experimental_resource() -> Json<Value> {
    Json(json!({}))
}
async fn experimental_workspace() -> Json<Value> {
    Json(json!([]))
}
async fn formatter_list() -> Json<Value> {
    Json(json!([]))
}
async fn lsp_list() -> Json<Value> {
    Json(json!([]))
}
async fn project_directories() -> Json<Value> {
    Json(json!([]))
}

/// GET /mcp — {name: {status, error?}} (freeze capture §1146).
async fn get_mcp(State(st): State<Arc<AppState>>) -> Json<Value> {
    let hub = st
        .mcp
        .get()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(refine_mcp::McpHub::default()));
    Json(hub.statuses())
}

/// GET /experimental/session?search=&roots=&limit= — session title search
/// (oc-remote searchSessions: "not a content search"); rows = list wire shape
/// + embedded project {id, worktree} (observed contract, manifest keys mode).
async fn get_experimental_sessions(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let mut v = refine_store::load_sessions_wire(&st.db).unwrap_or_else(|e| {
        tracing::error!("session list read failed: {e:#}");
        Vec::new()
    });
    if let Some(search) = q.get("search").filter(|s| !s.is_empty()) {
        let needle = search.to_lowercase();
        v.retain(|s| {
            s["title"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&needle)
        });
    }
    if let Some(limit) = q.get("limit").and_then(|l| l.parse::<usize>().ok()) {
        v.truncate(limit);
    }
    // roots=true is implicit: our store holds root sessions only (M3 scope).
    for row in &mut v {
        row["project"] = json!({"id": "global", "worktree": "/"});
    }
    Json(Value::Array(v))
}

/// POST /permission/{id}/reply — {reply: once|always|reject} (oc-remote contract).
async fn post_permission_reply(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let reply = body
        .get("reply")
        .and_then(|v| v.as_str())
        .unwrap_or("reject")
        .to_string();
    if !st.gate.reply(&id, &reply) {
        return Err(ApiError::not_found(format!(
            "Permission request not found: {id}"
        )));
    }
    Ok(Json(json!({})))
}

/// GET /permission — pending permission requests.
async fn get_permissions(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(serde_json::Value::Array(st.gate.list()))
}

/// GET /vcs — {branch, default_branch} (values cwd-dependent; keys golden).
async fn vcs_info() -> impl IntoResponse {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    Json(json!({
        "branch": git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        "default_branch": git(&["symbolic-ref", "refs/remotes/origin/HEAD"])
            .map(|r| r.trim_start_matches("refs/remotes/origin/").to_string()),
    }))
}

async fn get_sessions(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    // Fresh DB rows (upstream semantics) — the boot map was stale after
    // prompts (M2 finding); load_sessions_wire orders by time_updated DESC.
    let v = refine_store::load_sessions_wire(&st.db).unwrap_or_else(|e| {
        tracing::error!("session list read failed: {e:#}");
        Vec::new()
    });
    Json(v)
}

async fn get_session(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    refine_store::load_session_wire(&st.db, &id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

async fn session_status() -> impl IntoResponse {
    Json(json!({}))
}

/// GET /path — env-derived at boot (upstream keys: home/state/config/worktree/directory).
async fn get_path(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.paths.clone())
}

/// GET /project — global project + current worktree project (keys: id/worktree/time/sandboxes).
async fn get_projects(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let worktree = st.paths["worktree"].as_str().unwrap_or("/").to_string();
    let project = |id: &str, wt: &str| {
        json!({
            "id": id,
            "worktree": wt,
            "time": {"created": now_ms, "updated": now_ms, "initialized": now_ms},
            "sandboxes": [],
        })
    };
    Json(json!([
        project("global", "/"),
        project("current", &worktree),
    ]))
}

/// GET /project/current — same shape, single object.
async fn get_project_current(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let worktree = st.paths["worktree"].as_str().unwrap_or("/");
    Json(json!({
        "id": "global",
        "worktree": worktree,
        "time": {"created": now_ms, "updated": now_ms, "initialized": now_ms},
        "sandboxes": [],
    }))
}

/// Prometheus text format for the KPI series (SRE §2). Internal bind only.
async fn metrics(State(_st): State<Arc<AppState>>) -> impl IntoResponse {
    // registry counters are incremented by the timing middleware; rss sampled
    // here (scrape-time) so gauges are fresh
    refine_metrics::sample_rss();
    let body = refine_metrics::render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

// ---- M2: session create + sync prompt + message list ----

/// Two-word slug (upstream generates like "stellar-orchid" — entropy from id).
fn slug_for(id: &str) -> String {
    const A: &[&str] = &[
        "stellar", "amber", "quiet", "cobalt", "rapid", "lunar", "vivid", "crisp", "amber", "ivory",
    ];
    const B: &[&str] = &[
        "orchid", "falcon", "meadow", "canyon", "ember", "drift", "quartz", "harbor", "summit",
        "ripple",
    ];
    let h = id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    format!(
        "{}-{}",
        A[(h % A.len() as u64) as usize],
        B[((h >> 16) % B.len() as u64) as usize]
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// POST /session — create (wire shape from captured fixture: cost, directory,
/// id, path, projectID, slug, time, title, tokens, version).
async fn post_session(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let id = refine_core::ids::ses_id();
    let now = now_ms();
    let worktree = st.paths["worktree"].as_str().unwrap_or("/").to_string();
    let title = body
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let info = json!({
        "id": id,
        "projectID": "global",
        "directory": worktree,
        "path": worktree.trim_start_matches('/'),
        "slug": slug_for(&id),
        "title": title,
        "version": FREEZE_VERSION,
        "time": {"created": now, "updated": now},
        "cost": 0,
        "tokens": {"input": 0, "output": 0, "reasoning": 0,
                   "cache": {"read": 0, "write": 0}},
    });
    refine_store::insert_session(&st.writer, &info).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    st.bus.publish(refine_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "session.created",
        json!({"sessionID": id, "info": info}),
    ));
    Ok(Json(info))
}

/// GET /session/{id}/message — [{info, parts}] (captured wrapper shape).
async fn get_messages(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, HttpError> {
    // freeze parity: upstream rejects EVERY `before` value (§1090) with the
    // Effect HttpApi tagged body — not a divergence, a recorded fact.
    if q.contains_key("before") {
        return Err(HttpError::Tagged {
            status: StatusCode::BAD_REQUEST,
            tag: "BadRequest",
        });
    }
    if !refine_store::session_exists(&st.db, &id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")).into());
    }
    let limit: Option<usize> = q.get("limit").and_then(|l| l.parse::<usize>().ok());
    let msgs = refine_store::load_messages(&st.db, &id, limit).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    let arr: Vec<serde_json::Value> = msgs
        .into_iter()
        .map(|(info, parts)| json!({"info": info, "parts": parts}))
        .collect();
    Ok(Json(serde_json::Value::Array(arr)))
}

/// POST /session/{id}/message — sync prompt (blocks until stream completes,
/// returns {info, parts}; captured in testdata/m2/prompt_response.json).
/// Resolve agent/model/system/endpoint/rules into a runnable prompt context.
/// Shared by POST /message (sync) and POST /prompt_async (backgrounded).
fn build_prompt_context(
    st: &Arc<AppState>,
    payload: &Value,
) -> Result<refine_core::prompt::PromptContext, ApiError> {
    let agent = payload
        .get("agent")
        .and_then(|a| a.as_str())
        .filter(|a| !a.is_empty())
        .map(String::from)
        .unwrap_or_else(|| st.llm.default_agent.clone());
    let system = st
        .llm
        .systems
        .get(&agent)
        .cloned()
        .unwrap_or_else(|| crate::BUILD_SYSTEM_BLURB.to_string());
    let (pid, mid) = match (
        payload
            .pointer("/model/providerID")
            .and_then(|v| v.as_str()),
        payload.pointer("/model/modelID").and_then(|v| v.as_str()),
    ) {
        (Some(p), Some(m)) => (p.to_string(), m.to_string()),
        _ => st.llm.default_model.clone(),
    };
    let (base_url, api_key) = st
        .llm
        .endpoints
        .get(&pid)
        .cloned()
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: format!("no endpoint configured for provider {pid}"),
        })?;
    let pricing = st.llm.pricing.get(&(pid.clone(), mid.clone())).copied();
    // agent permission rules (v1 wire shape → evaluator)
    let rules: Vec<refine_tools::Rule> = st
        .agent
        .iter()
        .find(|a| a["name"].as_str() == Some(agent.as_str()))
        .and_then(|a| a["permission"].as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(refine_tools::Rule {
                        permission: r["permission"].as_str()?.to_string(),
                        pattern: r["pattern"].as_str()?.to_string(),
                        action: r["action"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let ctx = refine_core::prompt::PromptContext {
        db: st.db.clone(),
        blobs: st.blobs.clone(),
        bus: st.bus.clone(),
        directory: st.paths["directory"].as_str().unwrap_or("/").to_string(),
        agent: agent.clone(),
        system,
        endpoint: refine_core::prompt::LlmEndpoint {
            base_url,
            api_key,
            pricing,
        },
        model_id: mid,
        provider_id: pid,
        rules,
        gate: st.gate.clone(),
        mcp: st.mcp.get().cloned(),
        plugins: st.plugins.get().cloned(),
    };
    Ok(ctx)
}

/// Per-session prompt lock (insert-only while active; bounded by in-flight).
async fn lock_session(
    st: &Arc<AppState>,
    sid: &str,
) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>, ApiError> {
    let lock = {
        let mut map = st.prompt_locks.lock();
        if map.len() >= 64 && !map.contains_key(sid) {
            return Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                name: "ServiceUnavailableError",
                message: "too many concurrent prompts".into(),
            });
        }
        map.entry(sid.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    Ok(lock)
}

fn prompt_err(e: anyhow::Error) -> ApiError {
    let msg = format!("{e:#}");
    if msg.starts_with("Session not found") {
        ApiError::not_found(msg)
    } else {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: msg,
        }
    }
}

async fn post_message(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_prompt_context(&st, &payload)?;
    let lock = lock_session(&st, &id).await?;
    let _guard = lock.lock().await;
    let writer = st.writer.clone();
    let result = refine_core::prompt::run_prompt(&ctx, &writer, &id, &payload)
        .await
        .map_err(prompt_err)?;
    drop(_guard);
    {
        let mut map = st.prompt_locks.lock();
        if let Some(arc) = map.get(&id)
            && std::sync::Arc::strong_count(arc) <= 2
        {
            map.remove(&id);
        }
    }
    let (info, parts) = result;
    Ok(Json(json!({"info": info, "parts": parts})))
}

/// POST /session/{id}/prompt_async — v1 `session.prompt_async` (oc-remote's
/// send path): same PromptPayload as /message, **204 immediately**, prompt
/// runs in the background; results arrive on SSE. Errors: NotFound (bad
/// session) / BadRequest before spawn; background failures are logged
/// (upstream handlers/session.ts logs "prompt_async failed", no reply body).
async fn post_prompt_async(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<StatusCode, ApiError> {
    if !refine_store::session_exists(&st.db, &id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let ctx = build_prompt_context(&st, &payload)?;
    let lock = lock_session(&st, &id).await?;
    let writer = st.writer.clone();
    let locks = st.prompt_locks.clone();
    let sid = id.clone();
    let generation = st
        .prompt_gen
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tasks = st.prompt_tasks.clone();
    let tasks_insert = st.prompt_tasks.clone();
    let handle = tokio::spawn(async move {
        let guard = lock.lock().await;
        match refine_core::prompt::run_prompt(&ctx, &writer, &sid, &payload).await {
            Ok(_) => {}
            Err(e) => tracing::error!("prompt_async failed session={sid}: {e:#}"),
        }
        drop(guard);
        {
            let mut map = locks.lock();
            if let Some(arc) = map.get(&sid)
                && std::sync::Arc::strong_count(arc) <= 2
            {
                map.remove(&sid);
            }
        }
        let mut t = tasks.lock();
        if let Some((g, _)) = t.get(&sid)
            && *g == generation
        {
            t.remove(&sid);
        }
    });
    tasks_insert.lock().insert(id.clone(), (generation, handle));
    Ok(StatusCode::NO_CONTENT)
}

// ---- oc-remote contract family (Batch 1: session/message/part) ----

async fn get_children(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    refine_store::load_children(&st.db, &id)
        .map(|v| Json(Value::Array(v)))
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })
}

async fn get_todos(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    refine_store::load_todos(&st.db, &id)
        .map(|v| Json(Value::Array(v)))
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })
}

/// PATCH /session/{id} — oc-remote rename sends {title} only. Other
/// UpdatePayload fields (metadata/permission/time.archived) are not stored
/// by refine — accepted and ignored (documented divergence).
async fn patch_session(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if let Some(title) = payload.get("title").and_then(|t| t.as_str()) {
        refine_store::update_session_title(&st.writer, &id, title).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?;
    }
    refine_store::load_session_wire(&st.db, &id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// DELETE /session/{id} → true (v1 Schema.Boolean). Aborts any in-flight
/// async prompt first (it would write into a deleted session otherwise).
async fn delete_session_route(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    if let Some((_, handle)) = st.prompt_tasks.lock().remove(&id) {
        handle.abort();
    }
    refine_store::delete_session(&st.writer, &st.db, &id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .then_some(Json(json!(true)))
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// POST /session/{id}/abort → true (idempotent: no active task = still true).
async fn post_abort(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    if let Some((_, handle)) = st.prompt_tasks.lock().remove(&id) {
        handle.abort();
        tracing::info!("abort: cancelled prompt task for {id}");
    }
    Ok(Json(json!(true)))
}

async fn delete_message_route(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((sid, mid)): axum::extract::Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    refine_store::delete_message(&st.writer, &st.db, &sid, &mid)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .then_some(Json(json!(true)))
        .ok_or_else(|| ApiError::not_found(format!("Message not found: {mid}")))
}

async fn delete_part_route(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((_sid, mid, pid)): axum::extract::Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    refine_store::delete_part(&st.writer, &st.db, &mid, &pid)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .then_some(Json(json!(true)))
        .ok_or_else(|| ApiError::not_found(format!("Part not found: {pid}")))
}

/// PATCH .../part/{pid} — replace part JSON (native sessionID/messageID keys;
/// v1 verifies all three ids match the path — oc-remote pre-validates too).
async fn patch_part_route(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((sid, mid, pid)): axum::extract::Path<(String, String, String)>,
    Json(part): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let body_sid = part.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
    let body_mid = part.get("messageID").and_then(|v| v.as_str()).unwrap_or("");
    let body_pid = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if body_sid != sid || body_mid != mid || body_pid != pid {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "part id/sessionID/messageID must match the path".into(),
        });
    }
    refine_store::update_part(&st.writer, &st.blobs, &mid, &pid, &part)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .then_some(Json(part))
        .ok_or_else(|| ApiError::not_found(format!("Part not found: {pid}")))
}

// ---- oc-remote contract family (Batch 2: file/find/question) ----

fn scope_path(base: &std::path::Path, raw: &str) -> Result<std::path::PathBuf, ApiError> {
    let joined = if raw.is_empty() || raw == "." {
        base.to_path_buf()
    } else {
        let p = std::path::Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        }
    };
    let canon_base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let canon = std::fs::canonicalize(&joined).unwrap_or(joined.clone());
    if !canon.starts_with(&canon_base) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: format!("path escapes the session directory: {raw}"),
        });
    }
    Ok(canon)
}

/// GET /file?path= — directory listing (FileNode list, oc-remote file browser).
async fn list_directory(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/"));
    let raw = q.get("path").map(String::as_str).unwrap_or(".");
    let dir = scope_path(base, raw)?;
    let rd = std::fs::read_dir(&dir).map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: format!("list {}: {e}", dir.display()),
    })?;
    let mut nodes: Vec<Value> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let rel = if raw.is_empty() || raw == "." {
            name.clone()
        } else {
            format!("{}/{}", raw.trim_end_matches('/'), name)
        };
        let ignored =
            name.starts_with('.') || name == "node_modules" || name == "target" || name == ".git";
        nodes.push(json!({
            "name": name,
            "path": rel,
            "type": if is_dir { "directory" } else { "file" },
            "absolute": path.to_string_lossy(),
            "ignored": ignored,
            "size": meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
            "modified": meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
            }),
        }));
    }
    // directories first, then name (browser expectations), bounded
    nodes.sort_by(|a, b| {
        let da = a["type"] == "directory";
        let db = b["type"] == "directory";
        db.cmp(&da)
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    nodes.truncate(5000);
    Ok(Json(Value::Array(nodes)))
}

/// GET /file/content?path= — {type: text|binary, content, encoding}.
async fn read_file_content(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/"));
    let raw = q.get("path").map(String::as_str).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: "missing path".into(),
    })?;
    let path = scope_path(base, raw)?;
    let bytes = std::fs::read(&path).map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: format!("read {}: {e}", path.display()),
    })?;
    // bound: MEMORY §6 parse cap — truncate at 1MB
    let truncated = bytes.len() > 1024 * 1024;
    let bytes = if truncated {
        &bytes[..1024 * 1024]
    } else {
        &bytes[..]
    };
    let (kind, content) = match std::str::from_utf8(bytes) {
        Ok(text) => ("text", text.to_string()),
        Err(_) => ("binary", String::from_utf8_lossy(bytes).into_owned()),
    };
    Ok(Json(json!({
        "type": kind,
        "content": content,
        "encoding": "utf-8",
    })))
}

/// GET /find/file?query=&type=&limit= — file-name/path search → List<String>
/// (bounded walk; case-insensitive substring, glob when the query has *).
async fn find_files(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/"));
    let query = q
        .get("query")
        .map(String::as_str)
        .unwrap_or("")
        .to_lowercase();
    if query.is_empty() {
        return Ok(Json(json!([])));
    }
    let want_type = q.get("type").map(String::as_str).unwrap_or("");
    let limit: usize = q
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let glob_mode = query.contains('*') || query.contains('?');
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![(base.to_path_buf(), String::new())];
    let mut visited = 0usize;
    while let Some((dir, rel)) = stack.pop() {
        visited += 1;
        if visited > 20_000 || out.len() >= limit * 4 {
            break; // bounded (AGENTS §2.3)
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let matched = if glob_mode {
                refine_tools::wildcard_match(&name, &query)
            } else {
                child_rel.to_lowercase().contains(&query)
            };
            if matched
                && (want_type.is_empty()
                    || (want_type == "dir" && is_dir)
                    || (want_type == "file" && !is_dir))
            {
                out.push(child_rel.clone());
            }
            if is_dir {
                stack.push((entry.path(), child_rel));
            }
        }
    }
    // prefix matches first, then lexicographic; capped
    out.sort_by(|a, b| {
        let pa = a.to_lowercase().starts_with(&query);
        let pb = b.to_lowercase().starts_with(&query);
        pb.cmp(&pa).then_with(|| a.cmp(b))
    });
    out.truncate(limit);
    Ok(Json(Value::Array(
        out.into_iter().map(Value::String).collect(),
    )))
}

/// GET /find?pattern= — text search → SearchMatch list. Single `grep -rnE`
/// spawn (bounded output, 5s); absoluteOffset is not derivable from grep
/// output and this route has no callers in oc-remote — recorded as 0
/// (documented, not silently invented).
async fn find_text(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/"));
    let pattern = q.get("pattern").map(String::as_str).unwrap_or("");
    if pattern.is_empty() {
        return Ok(Json(json!([])));
    }
    let out = std::process::Command::new("grep")
        .args([
            "-rnE",
            "--binary-files=without-match",
            "-m",
            "50",
            "--",
            pattern,
        ])
        .arg(base)
        .env("LC_ALL", "C")
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            return Err(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                name: "InternalError",
                message: format!("grep spawn: {e}"),
            });
        }
    };
    // grep: 0=matches, 1=none, >1=error (e.g. bad pattern → 2 → 400)
    if out.status.code().unwrap_or(0) > 1 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "invalid pattern".into(),
        });
    }
    let mut matches: Vec<Value> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines().take(200) {
        // path:line:content
        let (p1, rest) = match line.split_once(':') {
            Some(x) => x,
            None => continue,
        };
        let (ln, text) = match rest.split_once(':') {
            Some(x) => x,
            None => continue,
        };
        let ln: i64 = ln.parse().unwrap_or(0);
        let rel = std::path::Path::new(p1)
            .strip_prefix(base)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| p1.to_string());
        matches.push(json!({
            "path": rel,
            "lines": text,
            "lineNumber": ln,
            "absoluteOffset": 0,
        }));
    }
    Ok(Json(Value::Array(matches)))
}

/// GET /question — pending questions. refine has no question tool yet → []
/// (ConnectionService polls this; empty list is the honest answer).
async fn get_questions() -> Json<Value> {
    Json(json!([]))
}

/// POST /question/{id}/reply|reject → true; no questions can be pending yet
/// → 404 (freeze NotFound shape), never a silent success.
async fn question_missing(id: axum::extract::Path<String>) -> Result<Json<Value>, ApiError> {
    Err(ApiError::not_found(format!("Question not found: {}", id.0)))
}

/// Freeze-shaped event id: `evt_` + 26 hex chars (upstream ids are time+random
/// 26-char alphanumerics; ours hash a nanos counter — clients only require
/// uniqueness, and byte-golden normalizes this span).
fn evt_id(kind: &str) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(kind.as_bytes());
    let hex = hex::encode(h.finalize());
    format!("evt_{}", &hex[..26])
}

/// SSE endpoint. Freeze facts (byte-golden capture, PLAN §3):
/// - response headers: Cache-Control: no-cache, no-transform; X-Content-Type-Options: nosniff;
///   x-accel-buffering: no; Vary: Origin; chunked transfer
/// - NO `id:` lines, NO `retry:` lines
/// - first frame: server.connected WITHOUT directory/project wrapper
/// - then 10s JSON heartbeats
/// - frames wrapped: {"directory":..,"project":..,"payload":..} (except server.connected)
async fn global_event(State(st): State<Arc<AppState>>) -> Response {
    let connected = Event::default().data(
        json!({"payload":{"id":evt_id("connected"),"type":"server.connected","properties":{}}})
            .to_string(),
    );
    let rx = st.bus.subscribe();
    refine_metrics::gauge(
        "refine_sse_clients",
        st.bus.subscriber_count().max(1) as i64,
    );
    // The stream must own a bus clone: if the Router/state were dropped while
    // a subscriber still streams (oneshot tests), the Sender must survive.
    let bus_keepalive = st.bus.clone();
    // Drop-guard: decrement the client gauge when the stream ends (any exit).
    struct SseClientGuard;
    impl Drop for SseClientGuard {
        fn drop(&mut self) {
            refine_metrics::gauge_delta("refine_sse_clients", -1);
        }
    }
    let sse_guard = SseClientGuard;
    // Timeout-driven merge: heartbeat at exact 10s cadence regardless of
    // traffic; bus frames yielded as they arrive; Lagged/Closed ends the
    // stream (bounded-queue disconnect, PLAN §4).
    let events = stream::once(async move { Ok::<_, std::convert::Infallible>(connected) }).chain(
        stream::unfold(
            (
                rx,
                tokio::time::Instant::now() + Duration::from_secs(10),
                bus_keepalive,
                sse_guard,
            ),
            |(mut rx, mut next_hb, bus, guard)| async move {
                {
                    let dur = next_hb.saturating_duration_since(tokio::time::Instant::now());
                    match tokio::time::timeout(dur.max(Duration::from_millis(1)), rx.recv()).await {
                        Ok(Ok(frame)) => {
                            refine_metrics::counter("refine_sse_events_total", 1);
                            Some((
                                Ok(Event::default().data(frame.to_string())),
                                (rx, next_hb, bus, guard),
                            ))
                        }
                        Ok(Err(_lagged_or_closed)) => {
                            refine_metrics::labeled_counter(
                                "refine_event_ring_lag_total",
                                "reason=\"lagged_or_closed\"",
                                1,
                            );
                            None
                        }
                        Err(_elapsed) => {
                            let hb = Event::default().data(
                                json!({"payload":{"id":evt_id("heartbeat"),
                                    "type":"server.heartbeat","properties":{}}})
                                .to_string(),
                            );
                            next_hb += Duration::from_secs(10);
                            Some((Ok(hb), (rx, next_hb, bus, guard)))
                        }
                    }
                }
            },
        ),
    );
    // No axum KeepAlive: the freeze stream carries zero comment/retry frames
    // (PLAN §3 byte-golden); the 10s heartbeat payload is the only activity.
    let mut resp = Sse::new(events).into_response();
    // Freeze header set — axum's defaults differ (no-transform / nosniff / accel).
    let h = resp.headers_mut();
    for (k, v) in sse_expected_headers() {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::try_from(k),
            HeaderValue::from_str(v),
        ) {
            h.insert(name, val);
        }
    }
    resp
}

/// Basic auth middleware (N6/PLAN §3: auth mode is a freeze artifact — live
/// instance is passwordless, so default is off; `basic` mode 401s everything
/// without matching credentials, including SSE and /metrics).
async fn auth_gate(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, ApiError> {
    st.requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SRE §2: request metrics (bounded route labels; measured at both exits)
    refine_metrics::counter("refine_requests_total", 1);
    let metric_t0 = std::time::Instant::now();
    let metric_label = format!(
        "route=\"{}\",method=\"{}\"",
        refine_metrics::route_label(req.uri().path()),
        req.method()
    );
    let record = move |t0: std::time::Instant| {
        refine_metrics::observe(
            "refine_http_request_duration_seconds",
            &metric_label,
            t0.elapsed().as_micros() as u64,
        );
    };
    // Hit-set capture (M1 §PLAN C4): every request path logged for the
    // differential route inventory.
    tracing::info!("req {} {}", req.method(), req.uri().path());
    if let Some((user, pass)) = &st.auth {
        let authorized = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b64| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            })
            .map(|raw| {
                let text = String::from_utf8_lossy(&raw);
                text == format!("{user}:{pass}")
            })
            .unwrap_or(false);
        if !authorized {
            record(metric_t0);
            return Err(ApiError {
                status: StatusCode::UNAUTHORIZED,
                name: "UnauthorizedError",
                message: "unauthorized".into(),
            });
        }
    }
    let resp = next.run(req).await;
    record(metric_t0);
    Ok(resp)
}

/// Router for the P0 surface. Auth is an AppState decision (freeze §3).
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/global/health", get(health))
        .route("/config", get(get_config))
        .route("/config/providers", get(get_config_providers))
        .route("/provider", get(get_provider))
        .route("/agent", get(get_agent))
        .route("/command", get(get_command))
        .route("/experimental/console", get(get_console))
        .route("/experimental/capabilities", get(get_capabilities))
        .route("/path", get(get_path))
        .route("/project", get(get_projects))
        .route("/project/current", get(get_project_current))
        .route("/session", get(get_sessions).post(post_session))
        .route("/session/status", get(session_status))
        .route(
            "/session/{id}",
            get(get_session)
                .patch(patch_session)
                .delete(delete_session_route),
        )
        .route(
            "/session/{id}/message",
            get(get_messages).post(post_message),
        )
        .route("/global/event", get(global_event))
        .route("/metrics", get(metrics))
        // TUI-attach probes (captured live; PLAN §2 hit-set expansion)
        .route("/api/location", get(get_api_location))
        .route("/api/agent", get(get_api_agent))
        .route("/api/command", get(get_api_command))
        .route("/api/reference", get(get_api_reference))
        .route("/experimental/resource", get(experimental_resource))
        .route("/experimental/workspace", get(experimental_workspace))
        .route("/formatter", get(formatter_list))
        .route("/lsp", get(lsp_list))
        .route("/project/{id}/directories", get(project_directories))
        .route("/vcs", get(vcs_info))
        .route(
            "/session/{id}/prompt_async",
            axum::routing::post(post_prompt_async),
        )
        .route("/session/{id}/children", get(get_children))
        .route("/file", get(list_directory))
        .route("/file/content", get(read_file_content))
        .route("/find/file", get(find_files))
        .route("/find", get(find_text))
        .route("/question", get(get_questions))
        .route(
            "/question/{id}/reply",
            axum::routing::post(question_missing),
        )
        .route(
            "/question/{id}/reject",
            axum::routing::post(question_missing),
        )
        .route("/session/{id}/todo", get(get_todos))
        .route("/session/{id}/abort", axum::routing::post(post_abort))
        .route(
            "/session/{id}/message/{mid}",
            axum::routing::delete(delete_message_route),
        )
        .route(
            "/session/{id}/message/{mid}/part/{pid}",
            axum::routing::delete(delete_part_route).patch(patch_part_route),
        )
        .route("/experimental/session", get(get_experimental_sessions))
        .route("/mcp", get(get_mcp))
        .route("/permission", get(get_permissions))
        .route(
            "/permission/{id}/reply",
            axum::routing::post(post_permission_reply),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_gate,
        ))
        .with_state(state)
}

/// JSON key-path projection: `$`, `$.a`, `$.a[].b`, … (values ignored).
/// Shared oracle for keys-mode golden comparisons (manifest mode=keys).
pub fn keypaths(v: &serde_json::Value, prefix: &str, out: &mut std::collections::BTreeSet<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m {
                let p = format!("{prefix}.{k}");
                out.insert(p.clone());
                keypaths(val, &p, out);
            }
        }
        serde_json::Value::Array(a) => {
            let p = format!("{prefix}[]");
            if let Some(first) = a.first() {
                keypaths(first, &p, out);
            } else {
                out.insert(p);
            }
        }
        _ => {}
    }
}

/// Header values the SSE route must carry (asserted by tests against golden bytes).
pub fn sse_expected_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("cache-control", "no-cache, no-transform"),
        ("x-content-type-options", "nosniff"),
        ("content-type", "text/event-stream"),
        ("x-accel-buffering", "no"),
    ]
}

#[allow(dead_code)]
fn _assert_header_value(v: &str) -> HeaderValue {
    HeaderValue::from_str(v).expect("static header value")
}
