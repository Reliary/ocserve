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
async fn metrics(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let rss = read_vm_rss_kb();
    let body = format!(
        "# HELP refine_requests_total HTTP requests handled\n\
         # TYPE refine_requests_total counter\n\
         refine_requests_total {}\n\
         # HELP refine_rss_bytes resident set size\n\
         # TYPE refine_rss_bytes gauge\n\
         refine_rss_bytes {}\n",
        st.requests.load(std::sync::atomic::Ordering::Relaxed),
        rss * 1024,
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

fn read_vm_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
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
) -> Result<impl IntoResponse, ApiError> {
    if !refine_store::session_exists(&st.db, &id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let msgs = refine_store::load_messages(&st.db, &id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    let out: Vec<Value> = msgs
        .into_iter()
        .map(|(info, parts)| json!({"info": info, "parts": parts}))
        .collect();
    Ok(Json(out))
}

/// POST /session/{id}/message — sync prompt (blocks until stream completes,
/// returns {info, parts}; captured in testdata/m2/prompt_response.json).
async fn post_message(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
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
    };
    let writer = st.writer.clone();
    let result = refine_core::prompt::run_prompt(&ctx, &writer, &id, &payload)
        .await
        .map_err(|e| {
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
        })?;
    let (info, parts) = result;
    Ok(Json(json!({"info": info, "parts": parts})))
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
    // The stream must own a bus clone: if the Router/state were dropped while
    // a subscriber still streams (oneshot tests), the Sender must survive.
    let bus_keepalive = st.bus.clone();
    // Timeout-driven merge: heartbeat at exact 10s cadence regardless of
    // traffic; bus frames yielded as they arrive; Lagged/Closed ends the
    // stream (bounded-queue disconnect, PLAN §4).
    let events = stream::once(async move { Ok::<_, std::convert::Infallible>(connected) }).chain(
        stream::unfold(
            (
                rx,
                tokio::time::Instant::now() + Duration::from_secs(10),
                bus_keepalive,
            ),
            |(mut rx, mut next_hb, bus)| async move {
                {
                    let dur = next_hb.saturating_duration_since(tokio::time::Instant::now());
                    match tokio::time::timeout(dur.max(Duration::from_millis(1)), rx.recv()).await {
                        Ok(Ok(frame)) => Some((
                            Ok(Event::default().data(frame.to_string())),
                            (rx, next_hb, bus),
                        )),
                        Ok(Err(_lagged_or_closed)) => None,
                        Err(_elapsed) => {
                            let hb = Event::default().data(
                                json!({"payload":{"id":evt_id("heartbeat"),
                                    "type":"server.heartbeat","properties":{}}})
                                .to_string(),
                            );
                            next_hb += Duration::from_secs(10);
                            Some((Ok(hb), (rx, next_hb, bus)))
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
            return Err(ApiError {
                status: StatusCode::UNAUTHORIZED,
                name: "UnauthorizedError",
                message: "unauthorized".into(),
            });
        }
    }
    Ok(next.run(req).await)
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
        .route("/session/{id}", get(get_session))
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
