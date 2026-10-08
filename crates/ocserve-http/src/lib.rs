//! ocserve-http: axum router, auth, byte-golden SSE, upstream error envelope.

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

/// Pre-assembled payloads for config-derived routes (built by ocserve's
/// `Runtime::load()`; kept crate-local so http doesn't depend on cli).
/// JSON deep merge: objects merge recursively, everything else (arrays,
/// scalars) replaces. Used by overlay layering + PATCH /config (W4).
pub fn deep_merge(base: &mut serde_json::Value, patch: serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(&k) {
                    Some(bv) => deep_merge(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, p) => *b = p,
    }
}

/// W4 config-write target: default = the SHARED opencode.json (drop-in
/// contract — both servers read it); `OCSERVE_CONFIG_WRITE=overlay` switches
/// to a ocserve-owned file that layers over the base at load (never touches
/// the other server's config).
pub fn config_write_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    if std::env::var("OCSERVE_CONFIG_WRITE")
        .map(|v| v == "overlay")
        .unwrap_or(false)
    {
        std::path::PathBuf::from(home).join(".config/ocserve/config.json")
    } else {
        std::path::PathBuf::from(home).join(".config/opencode/opencode.json")
    }
}

/// Boot-injected config reloader (W4): Runtime::load lives in ocserve —
/// the closure avoids a crate cycle; None in tests. Returns derived payloads
/// AND a rebuilt LLM registry (H2 — auth/provider/default-model edits apply
/// without restart). Callers go through `watch::reconcile`, never the raw fn.
pub type ConfigReloader =
    std::sync::Arc<dyn Fn() -> anyhow::Result<(Payloads, LlmRegistry)> + Send + Sync>;

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
    /// shared-config `compaction` section (M6 trigger/knobs).
    pub compaction: serde_json::Value,
}

/// PERF-10X F1: run a blocking store call on the runtime's blocking pool.
/// Store fns are sync by design (STORAGE: never hold a connection across
/// `.await`) — calling them inline parks a tokio worker for the query's
/// duration. Baseline measured exactly that convoy: one 6s search held a
/// worker and every other route queued behind it (config p95 998ms for a
/// payload read; starvation signature: tail explodes while CPU ≪100%).
async fn run_blocking<T, F>(db: &std::path::Path, op: F) -> anyhow::Result<T>
where
    F: FnOnce(&std::path::Path) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let db = db.to_path_buf();
    tokio::task::spawn_blocking(move || op(&db))
        .await
        .map_err(|e| anyhow::anyhow!("blocking task join: {e}"))?
}

pub mod compress;
pub mod pty;
pub mod tui;
pub mod ui;
pub mod vcs;

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
pub mod sync;
pub mod watch;

pub enum HttpError {
    Api(ApiError),
    Tagged {
        status: StatusCode,
        tag: &'static str,
    },
    /// Effect query-decode failure envelope (freeze probes: `?limit=abc` →
    /// {"name":"BadRequest","data":{"message":...,"kind":"Query"}}).
    Query {
        message: String,
    },
    /// Effect TaggedErrorClass envelope with schema fields at top level
    /// (errors.ts:143 McpServerNotFoundError → {"_tag":..., name, message}).
    TaggedData {
        status: StatusCode,
        tag: &'static str,
        fields: serde_json::Value,
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
            HttpError::TaggedData {
                status,
                tag,
                fields,
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("_tag".into(), serde_json::Value::String(tag.into()));
                if let serde_json::Value::Object(f) = fields {
                    obj.extend(f);
                }
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    serde_json::Value::Object(obj).to_string(),
                )
                    .into_response()
            }
            HttpError::Query { message } => {
                let body = serde_json::json!({
                    "name": "BadRequest",
                    "data": {"message": message, "kind": "Query"}
                })
                .to_string();
                (
                    StatusCode::BAD_REQUEST,
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
    /// Config-derived route payloads (W4: swappable after PATCH /config —
    /// upstream marks the instance for disposal and serves fresh config).
    pub payloads: parking_lot::RwLock<Payloads>,
    /// PERF-10X F5: pre-serialized bytes for the hot Json-clone handlers
    /// (config/agent/command/config_providers/provider/console/capabilities).
    /// Built at construction + every reload, so a request is a Bytes clone
    /// (refcount) + zero-copy Body — no per-request Value clone/serialize.
    /// Kill-switch: OCSERVE_WIRE_CACHE=0 (wire_off) falls back to the Value
    /// path; both paths serve serde-identical bytes (unit-tested).
    pub wire: parking_lot::RwLock<HashMap<&'static str, WireEntry>>,
    pub wire_off: std::sync::atomic::AtomicBool,
    /// Boot-injected config reloader (Runtime lives in ocserve; the
    /// closure avoids a crate cycle). None in tests → PATCH still writes
    /// the file, swap skipped (logged).
    pub reloader: parking_lot::RwLock<Option<ConfigReloader>>,
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
    pub bus: ocserve_core::EventBus,
    pub db: std::path::PathBuf,
    pub blobs: std::sync::Arc<ocserve_store::BlobStore>,
    pub writer: std::sync::Arc<ocserve_store::Writer>,
    /// Provider registry + default model + agent systems (from Runtime).
    /// RwLock since H2: rebuilds on config hot-reload (watch::reconcile);
    /// in-flight prompts keep the values they resolved at build time.
    pub llm: parking_lot::RwLock<LlmRegistry>,
    /// Config hot-reload watch state (H1): polled paths + observed tuples.
    pub watch: parking_lot::RwLock<watch::WatchState>,
    /// Serializes reconcile across PATCH/auth/watcher callers (tokio Mutex —
    /// held across reload + MCP awaits; never a parking_lot guard over await).
    pub reconcile_lock: tokio::sync::Mutex<()>,
    /// K-AUTONOMY knobs — read from env ONCE at construction (changing
    /// OCSERVE_PROMPT_MAX_ROUNDS / OCSERVE_PROMPT_MAX_COST_USD = restart).
    pub max_rounds: usize,
    pub cost_ceiling: f64,
    /// Permission rendezvous shared by runner + reply routes.
    pub gate: std::sync::Arc<ocserve_core::PermissionGate>,
    /// MCP hub probed at serve boot (OnceLock: tests run without probing).
    pub mcp: std::sync::OnceLock<std::sync::Arc<ocserve_mcp::McpHub>>,
    /// Plugin sidecar (M4b): async mutex — trigger() takes &mut across await.
    pub plugins: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<ocserve_plugin::Sidecar>>>,
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
    /// Question rendezvous (v1 Question service): pending asks + reply/reject.
    pub question_gate: std::sync::Arc<ocserve_core::question::QuestionGate>,
    /// /tui/control rendezvous queues (v1 shared/tui-control.ts)
    pub tui: crate::tui::TuiControl,
    /// Web-UI reverse proxy (Phase 7; upstream serveUIEffect parity)
    pub ui: crate::ui::UiProxy,
    /// PTY sessions (/pty/* — web-UI terminal)
    pub pty: std::sync::Arc<ocserve_pty::PtyManager>,
    /// User CORS allowlist (config `server.cors` + `--cors`), read once at boot
    pub cors_extra: Vec<String>,
}

/// LLM endpoint resolution for the prompt runner (assembled by Runtime).
#[derive(Default)]
pub struct LlmRegistry {
    /// providerID → (base_url, api_key)
    pub endpoints: std::collections::HashMap<String, (String, String)>,
    /// (providerID, modelID) → (input, output, cache_read) USD/MTok
    pub pricing: std::collections::HashMap<(String, String), (f64, f64, f64)>,
    /// (providerID, modelID) → `limit` block (context/input) — M6 trigger
    pub limits: std::collections::HashMap<(String, String), serde_json::Value>,
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
    pub blobs: std::sync::Arc<ocserve_store::BlobStore>,
    pub writer: std::sync::Arc<ocserve_store::Writer>,
    pub llm: LlmRegistry,
}

/// One wire-cache entry: identity bytes, precomputed compressed variants,
/// and a strong ETag over the identity body (WEBUI-PLAN W1/W2).
///
/// Compressed variants are built once per write-epoch (reload), never on the
/// request path — the provider payload is 6.2 MB and brotli-q5 costs ~0.18 s.
#[derive(Clone)]
pub struct WireEntry {
    pub identity: bytes::Bytes,
    pub br: bytes::Bytes,
    pub gzip: bytes::Bytes,
    pub etag: String,
}

/// Serialize the hot payload fields once (reload-time). off => empty map
/// (handlers fall back to the Value path).
pub(crate) fn rebuild_wire(p: &Payloads, off: bool) -> HashMap<&'static str, WireEntry> {
    let mut m = HashMap::new();
    if off {
        return m;
    }
    // serde_json::to_vec is exactly what axum's Json uses — byte-identical.
    fn one<T: serde::Serialize>(m: &mut HashMap<&'static str, WireEntry>, k: &'static str, v: &T) {
        if let Ok(b) = serde_json::to_vec(v) {
            let identity = bytes::Bytes::from(b);
            let etag = crate::compress::etag_for(&identity);
            let br = bytes::Bytes::from(crate::compress::compress(
                &identity,
                crate::compress::Encoding::Brotli,
            ));
            let gzip = bytes::Bytes::from(crate::compress::compress(
                &identity,
                crate::compress::Encoding::Gzip,
            ));
            m.insert(
                k,
                WireEntry {
                    identity,
                    br,
                    gzip,
                    etag,
                },
            );
        }
    }
    // /global/config == /config minus instance-scoped keys (freeze probe).
    {
        let mut gc = p.config.clone();
        if let Some(obj) = gc.as_object_mut() {
            for k in ["agent", "command", "mode", "username"] {
                obj.remove(k);
            }
        }
        one(&mut m, "global_config", &gc);
    }
    one(&mut m, "config", &p.config);
    one(&mut m, "agent", &p.agent);
    one(&mut m, "command", &p.command);
    one(&mut m, "config_providers", &p.config_providers);
    one(&mut m, "provider", &p.provider);
    one(&mut m, "console", &p.console);
    one(&mut m, "capabilities", &p.capabilities);
    m
}

/// Serve cached bytes as a JSON response with negotiation + validators; None
/// = fall back to the Value clone path (wire cache off / unknown key).
/// Identity clients (no Accept-Encoding, no If-None-Match) receive the exact
/// identity bytes and content-type as before (WEBUI-PLAN target 3).
fn wire_json_h(
    st: &AppState,
    key: &'static str,
    accept_encoding: Option<&str>,
    if_none_match: Option<&str>,
) -> Option<axum::response::Response> {
    if st.wire_off.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let e = st.wire.read().get(key).cloned()?;
    let encoding = crate::compress::negotiate(accept_encoding);
    // 304 short-circuit (RFC 9111): before any body work.
    if crate::compress::if_none_match_matches(if_none_match, &e.etag) {
        return Some(
            axum::response::Response::builder()
                .status(axum::http::StatusCode::NOT_MODIFIED)
                .header(axum::http::header::ETAG, &e.etag)
                .header(axum::http::header::VARY, "Accept-Encoding")
                .body(axum::body::Body::empty())
                .expect("static 304"),
        );
    }
    let mut b = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::ETAG, &e.etag)
        .header(axum::http::header::VARY, "Accept-Encoding");
    let body = match encoding {
        Some(crate::compress::Encoding::Brotli) => {
            b = b.header(axum::http::header::CONTENT_ENCODING, "br");
            e.br.clone()
        }
        Some(crate::compress::Encoding::Gzip) => {
            b = b.header(axum::http::header::CONTENT_ENCODING, "gzip");
            e.gzip.clone()
        }
        None => e.identity.clone(),
    };
    Some(
        b.body(axum::body::Body::from(body))
            .expect("static response"),
    )
}

/// Back-compat shim (tests + callers with no header context): identity only.
#[cfg(test)]
fn wire_json(st: &AppState, key: &'static str) -> Option<axum::response::Response> {
    wire_json_h(st, key, None, None)
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
            "ocserve-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp data dir");
        let db = ocserve_store::writer::db_path(&dir);
        let writer =
            std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).expect("temp writer"));
        let blobs = std::sync::Arc::new(
            ocserve_store::BlobStore::new(dir.join("blobs")).expect("temp blobs"),
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
        let wire_off = std::env::var("OCSERVE_WIRE_CACHE")
            .map(|v| v == "0")
            .unwrap_or(false);
        let wire_map = rebuild_wire(&p, wire_off);
        let bus = ocserve_core::EventBus::new();
        let pty = {
            let mgr = ocserve_pty::PtyManager::new(worktree.clone());
            let bus_for_pty = bus.clone();
            let dir_for_pty = worktree.clone();
            mgr.set_event_sink(std::sync::Arc::new(move |event_type, properties| {
                bus_for_pty.publish(ocserve_core::event::frame(
                    &dir_for_pty,
                    event_type,
                    properties,
                ));
            }));
            mgr
        };
        Arc::new(Self {
            payloads: parking_lot::RwLock::new(p),
            wire: parking_lot::RwLock::new(wire_map),
            wire_off: std::sync::atomic::AtomicBool::new(wire_off),
            reloader: parking_lot::RwLock::new(None),
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
            bus,
            gate: ocserve_core::PermissionGate::new(),
            mcp: std::sync::OnceLock::new(),
            plugins: std::sync::OnceLock::new(),
            prompt_locks: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            prompt_tasks: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            prompt_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            question_gate: ocserve_core::question::QuestionGate::new(),
            tui: crate::tui::TuiControl::default(),
            ui: crate::ui::UiProxy::from_env(),
            pty,
            cors_extra: read_cors_env(),
            db: w.db,
            blobs: w.blobs,
            writer: w.writer,
            llm: parking_lot::RwLock::new(w.llm),
            watch: parking_lot::RwLock::new(watch::WatchState::default()),
            reconcile_lock: tokio::sync::Mutex::new(()),
            max_rounds: std::env::var("OCSERVE_PROMPT_MAX_ROUNDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            cost_ceiling: std::env::var("OCSERVE_PROMPT_MAX_COST_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
        })
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({"healthy": true, "version": FREEZE_VERSION}))
}

async fn get_config(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "config", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().config.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

async fn get_agent(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "agent", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().agent.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

async fn get_command(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "command", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().command.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

async fn get_config_providers(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "config_providers", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().config_providers.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

async fn get_provider(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "provider", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().provider.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

async fn get_console(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "console", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().console.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

async fn get_capabilities(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    wire_json_h(&st, "capabilities", ae, inm).unwrap_or_else(|| {
        let v = st.payloads.read().capabilities.clone();
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
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
    let payloads = st.payloads.read();
    let data: Vec<Value> = payloads
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
    let payloads = st.payloads.read();
    let data: Vec<Value> = payloads
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
/// GET /experimental/tool?provider=&model= — v1 ToolList (groups/
/// experimental.ts:50-61; handlers/experimental.ts:94-105): [{id,
/// description, parameters}] = builtin + MCP tools. Effect validates the
/// query (missing provider/model → BadRequest) — mirrored here. Denied
/// tools excluded, matching what the model actually sees (W5).
async fn get_experimental_tool(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let has = |k: &str| q.get(k).map(|s| !s.is_empty()).unwrap_or(false);
    if !has("provider") || !has("model") {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "Invalid query: provider, model required".into(),
        });
    }
    let mut schemas = ocserve_tools::schemas();
    if let Some(hub) = st.mcp.get() {
        let mcp_tools =
            tokio::time::timeout(std::time::Duration::from_secs(10), hub.tool_schemas())
                .await
                .unwrap_or_else(|_| {
                    tracing::warn!("mcp tool schema fetch timed out (/experimental/tool)");
                    Vec::new()
                });
        schemas.extend(mcp_tools);
    }
    let items: Vec<Value> = schemas
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "id": f.get("name")?,
                "description": f.get("description")?,
                "parameters": f.get("parameters")?,
            }))
        })
        .collect();
    Ok(Json(Value::Array(items)))
}

/// GET /experimental/tool/ids — registry.ids(): plain string array.
async fn get_experimental_tool_ids() -> Json<Value> {
    let ids: Vec<Value> = ocserve_tools::schemas()
        .iter()
        .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
        .map(|n| json!(n))
        .collect();
    Json(Value::Array(ids))
}

async fn experimental_resource() -> Json<Value> {
    Json(json!({}))
}
async fn experimental_workspace() -> Json<Value> {
    Json(json!([]))
}

/// GET /experimental/workspace/status — freeze: `[]` 200 (probed live).
async fn experimental_workspace_status() -> Json<Value> {
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
        .unwrap_or_else(|| std::sync::Arc::new(ocserve_mcp::McpHub::default()));
    Json(hub.statuses())
}

/// GET /experimental/session?search=&roots=&limit= — session title search
/// (oc-remote searchSessions: "not a content search"); rows = list wire shape
/// + embedded project {id, worktree} (observed contract, manifest keys mode).
async fn get_experimental_sessions(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let mut v = run_blocking(&st.db, ocserve_store::load_sessions_wire)
        .await
        .unwrap_or_else(|e| {
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
async fn vcs_info(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let info = run_blocking(&st.db, move |_db| Ok::<_, anyhow::Error>(vcs::info(&dir)))
        .await
        .unwrap_or_else(|_| json!({"branch": null, "default_branch": null}));
    Json(info)
}

/// GET /vcs/status → FileStatus[] (freeze shape; [] when not a git repo).
async fn vcs_status(State(st): State<Arc<AppState>>) -> Response {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    match run_blocking(&st.db, move |_db| Ok::<_, anyhow::Error>(vcs::status(&dir))).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(_) => axum::Json(Value::Array(vec![])).into_response(),
    }
}

/// GET /vcs/diff?mode=git|branch[&context=N] → FileDiff[].
/// Missing/other mode → 400 Effect Query envelope (freeze probe).
async fn vcs_diff(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let mode = q.get("mode").map(String::as_str).unwrap_or("");
    if mode != "git" && mode != "branch" {
        return HttpError::Query {
            message: format!("Expected \"git\" | \"branch\", got \"{mode}\"\n  at [\"mode\"]"),
        }
        .into_response();
    }
    let context: Option<u32> = q.get("context").and_then(|v| v.parse().ok());
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let mode_owned = mode.to_string();
    let v = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(vcs::diff(&dir, &mode_owned, context))
    })
    .await
    .unwrap_or_else(|_| Value::Array(vec![]));
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let body = serde_json::to_vec(&v).unwrap_or_default();
    let etag = crate::compress::etag_for(&body);
    crate::compress::encode_response(
        StatusCode::OK,
        "application/json",
        bytes::Bytes::from(body),
        Some(&etag),
        ae,
        inm,
        &[],
    )
}

/// GET /vcs/diff/raw → raw patch text (empty string when clean).
async fn vcs_diff_raw(State(st): State<Arc<AppState>>) -> Response {
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    let text = run_blocking(&st.db, move |_db| {
        Ok::<_, anyhow::Error>(vcs::diff_raw(&dir))
    })
    .await
    .unwrap_or_default();
    Response::builder()
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .header(axum::http::header::VARY, "Accept-Encoding")
        .body(axum::body::Body::from(text))
        .expect("static response")
}

/// GET /file/status → [] (freeze returns the changed-file set from the
/// snapshot engine; with no snapshot engine the honest shape is empty —
/// the web UI only checks length>0 for a dirty badge, which `vcs/status`
/// now serves correctly).
async fn file_status() -> impl IntoResponse {
    axum::Json(Value::Array(vec![]))
}

/// GET /find/symbol → [] (freeze returns LSP symbols; no LSP server →
/// empty, the same shape clients tolerate).
async fn find_symbol() -> impl IntoResponse {
    axum::Json(Value::Array(vec![]))
}

/// GET /global/config → the effective merged config (freeze shape: same
/// keys as /config minus the per-instance `agent`/`command`/`mode`/`username`
/// overrides — probed key diff 2026-10-08). Served from the wire cache.
async fn global_config_get(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    // Freeze /global/config == /config with the instance-scoped keys absent.
    // Build it once per reload into the wire cache under "global_config".
    wire_json_h(&st, "global_config", ae, inm).unwrap_or_else(|| {
        let mut v = st.payloads.read().config.clone();
        if let Some(obj) = v.as_object_mut() {
            for k in ["agent", "command", "mode", "username"] {
                obj.remove(k);
            }
        }
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let etag = crate::compress::etag_for(&body);
        crate::compress::encode_response(
            StatusCode::OK,
            "application/json",
            bytes::Bytes::from(body),
            Some(&etag),
            ae,
            inm,
            &[],
        )
    })
}

// ---- PERF-10X F9: paged /message wire memo ----
//
// message_page was the CPU hog (route isolation: ~3 ms CPU/page — parse +
// merge + serialize per message, blocking-pool thread, channel framing).
// A paged response (limit>0, ≤50 messages) is assembled ONCE per
// (db, session, limit, cursor, write-epoch) and served as refcounted
// Bytes; hits skip reader, queries, parse, thread and channel entirely.
// Bound by construction: entries ≤ 4 MiB (larger assemblies abort and
// fall back to the original streaming path — the unbounded full-history
// stream is NEVER memoized, AGENTS §2.3), FIFO capped at 32 entries.
// Exactness: keys carry the write epoch; a writer bump invalidates
// (lazy clear on epoch mismatch — same rule as F7/F8). Kill switch:
// OCSERVE_PAGE_MEMO=0.
pub const PAGE_MEMO_ENTRY_CAP: usize = 4 * 1024 * 1024;
/// Count + BYTE bounds (F9c): the first cap (32) thrashed against the
/// fixture's 200-session spread working set (VU150/300 spread fell to
/// 3,794 rps, p95 278ms — every rotation missed and paid the full
/// assemble path; arrival never recovered from it). Working set ≈
/// sessions x pages-per-loop ≈ 600 keys; typical pages are 10-50KB so a
/// 64 MiB total budget holds the whole set (env: OCSERVE_PAGE_MEMO_MB).
pub const PAGE_MEMO_MAX_ENTRIES: usize = 2048;
pub const PAGE_MEMO_MAX_BYTES: usize = 64 * 1024 * 1024;

struct PageEntry {
    body: bytes::Bytes,
    next: Option<String>,
}

struct PageMemoState {
    epoch: u64,
    entries: std::collections::VecDeque<(String, PageEntry)>,
    bytes: usize,
}

impl PageMemoState {
    fn new() -> Self {
        Self {
            epoch: 0,
            entries: std::collections::VecDeque::new(),
            bytes: 0,
        }
    }
    fn get(&mut self, key: &str, epoch: u64) -> Option<PageEntry> {
        if self.epoch != epoch {
            self.entries.clear();
            self.bytes = 0;
            self.epoch = epoch;
        }
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, e)| PageEntry {
                body: e.body.clone(),
                next: e.next.clone(),
            })
    }
    fn put(&mut self, key: String, entry: PageEntry, epoch: u64) {
        if entry.body.len() > PAGE_MEMO_ENTRY_CAP {
            return;
        }
        if self.epoch != epoch {
            self.entries.clear();
            self.bytes = 0;
            self.epoch = epoch;
        }
        // drop same-key old copy (byte accounting)
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == &key) {
            let (_, old) = self.entries.remove(pos).expect("position found");
            self.bytes -= old.body.len();
        }
        let budget = std::env::var("OCSERVE_PAGE_MEMO_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(64)
            * 1024
            * 1024;
        self.bytes += entry.body.len();
        self.entries.push_back((key, entry));
        while self.entries.len() > PAGE_MEMO_MAX_ENTRIES
            || (self.bytes > budget && self.entries.len() > 1)
        {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.body.len();
            } else {
                break;
            }
        }
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
}

static PAGE_MEMO: std::sync::OnceLock<parking_lot::Mutex<PageMemoState>> =
    std::sync::OnceLock::new();
static PAGE_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PAGE_MEMO_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn page_memo() -> &'static parking_lot::Mutex<PageMemoState> {
    PAGE_MEMO.get_or_init(|| parking_lot::Mutex::new(PageMemoState::new()))
}

pub fn page_memo_enabled() -> bool {
    std::env::var("OCSERVE_PAGE_MEMO")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// (hits, misses, entries) for the load report / tests.
pub fn page_memo_stats() -> (u64, u64, usize) {
    (
        PAGE_MEMO_HITS.load(std::sync::atomic::Ordering::Relaxed),
        PAGE_MEMO_MISSES.load(std::sync::atomic::Ordering::Relaxed),
        page_memo().lock().len(),
    )
}

/// Page response builder shared by the memo path and the streaming path
/// (headers rule = upstream session.ts:133-147: Link echoes the request
/// origin — Host / x-forwarded-proto — so headers are rebuilt per request
/// even on a memo hit; only the BODY is cached).
fn page_response(
    headers: &axum::http::HeaderMap,
    uri: &axum::http::Uri,
    page_n: &Option<u64>,
    page_next: &Option<String>,
    body: axum::body::Body,
) -> Result<axum::response::Response, ApiError> {
    let mut builder = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if let (Some(n), Some(cur)) = (page_n, page_next) {
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");
        let link = format!(
            "<{scheme}://{host}{}?limit={n}&before={cur}>; rel=\"next\"",
            uri.path()
        );
        builder = builder
            .header("x-next-cursor", cur.as_str())
            .header("link", link)
            .header("access-control-expose-headers", "Link, X-Next-Cursor");
    }
    builder.body(body).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("response build: {e}"),
    })
}

async fn get_sessions(
    State(st): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    // W4: freeze ListQuery parity (limit default 100, roots, search, start,
    // directory, scope=project). The bare list keeps the exact single-slot
    // memo path (byte-identical); filtered variants use a param-keyed memo.
    // Fresh DB rows (upstream semantics) — the boot map was stale after
    // prompts (M2 finding); store orders by time_updated DESC.
    let query = ocserve_store::SessionListQuery {
        limit: q
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0),
        roots: q.get("roots").map(|v| v == "true").unwrap_or(false),
        search: q.get("search").filter(|s| !s.is_empty()).cloned(),
        start: q.get("start").and_then(|v| v.parse::<i64>().ok()),
        directory: q.get("directory").filter(|s| !s.is_empty()).cloned(),
        scope_project: q.get("scope").map(|v| v == "project").unwrap_or(false),
    };
    let ae = headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let qc = query.clone();
    match run_blocking(&st.db, move |db| {
        ocserve_store::load_sessions_wire_bytes_filtered(db, &qc)
    })
    .await
    {
        Ok(b) => {
            let etag = crate::compress::etag_for(&b);
            crate::compress::encode_response(
                axum::http::StatusCode::OK,
                "application/json",
                (*b).clone(),
                Some(&etag),
                ae,
                inm,
                &[],
            )
        }
        Err(e) => {
            tracing::error!("session list read failed: {e:#}");
            Json(Vec::<serde_json::Value>::new()).into_response()
        }
    }
}

async fn get_session(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let wire_id = id.clone();
    run_blocking(&st.db, move |db| {
        ocserve_store::load_session_wire(db, &wire_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?
    .map(Json)
    .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// GET /session/status — map of NON-idle sessions only (upstream
/// session/status.ts deletes idle entries; probe fixture: status_busy.body).
/// Busy = held/contended prompt lock OR a live background prompt task —
/// queued-but-not-yet-running prompts count (client's send is admitted).
async fn session_status(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let mut out = serde_json::Map::new();
    let mut busy = |sid: &str| {
        out.insert(sid.to_string(), serde_json::json!({"type": "busy"}));
    };
    for (sid, arc) in st.prompt_locks.lock().iter() {
        if arc.try_lock().is_err() {
            busy(sid);
        }
    }
    for (sid, (_gen, handle)) in st.prompt_tasks.lock().iter() {
        if !handle.is_finished() {
            busy(sid);
        }
    }
    Json(serde_json::Value::Object(out))
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
    ocserve_metrics::sample_rss();
    // Read-path fast activity (L1 splice + F5/F7/F8 memos), published as
    // absolute cumulative gauges at scrape time — these counters live in
    // ocserve-store as atomics with no event stream of their own.
    //  - splice_rows/fallbacks: the corpus gate proves 0 fallbacks on real
    //    data, so a non-zero fallback gauge means stored bytes hit a shape
    //    the splicer refuses — a signal, not a silent fallback (lib.rs L1).
    //  - memo hit/miss: the load report's cache-on/off variants must be
    //    observable in production, not just in a bench run.
    ocserve_metrics::gauge(
        "ocserve_splice_rows_total",
        ocserve_store::splice_rows() as i64,
    );
    ocserve_metrics::gauge(
        "ocserve_splice_fallbacks_total",
        ocserve_store::splice_fallbacks() as i64,
    );
    let (search_hits, search_misses, list_hits, list_misses, _pad, search_len, _epoch) =
        ocserve_store::memo_stats();
    ocserve_metrics::gauge("ocserve_memo_search_hits", search_hits as i64);
    ocserve_metrics::gauge("ocserve_memo_search_misses", search_misses as i64);
    ocserve_metrics::gauge("ocserve_memo_list_hits", list_hits as i64);
    ocserve_metrics::gauge("ocserve_memo_list_misses", list_misses as i64);
    ocserve_metrics::gauge("ocserve_memo_search_entries", search_len as i64);
    let body = ocserve_metrics::render();
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

/// ISO-8601 UTC with milliseconds (freeze `Date.prototype.toISOString`
/// shape) — days-from-civil, no time-crate dependency.
fn iso8601_z(ms: i64) -> String {
    let ms = ms.rem_euclid(86_400_000_000_000);
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (h, rem) = (rem.div_euclid(3_600_000), rem.rem_euclid(3_600_000));
    let (mi, rem) = (rem.div_euclid(60_000), rem.rem_euclid(60_000));
    let (sec, milli) = (rem.div_euclid(1_000), rem.rem_euclid(1_000));
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{sec:02}.{milli:03}Z")
}

/// Freeze default session title (session.ts `parentTitlePrefix + ISO`) —
/// empty titles would render "untitled" in clients (K-TITLE).
fn default_session_title(now_ms: i64) -> String {
    format!("New session - {}", iso8601_z(now_ms))
}

/// Freeze `getForkedTitle` (session.ts): `/^(.+) \(fork #(\d+)\)$/` →
/// `base (fork #(n+1))`, otherwise `title (fork #1)`. Ported with string ops
/// (regex `.+` needs ≥1 char before the suffix → `idx > 0`, greedy match →
/// last occurrence → `rfind`).
pub fn forked_title(title: &str) -> String {
    const PAT: &str = " (fork #";
    if let Some(idx) = title.rfind(PAT)
        && idx > 0
        && let Some(num) = title[idx + PAT.len()..].strip_suffix(')')
        && !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = num.parse::<u64>()
    {
        return format!("{} (fork #{})", &title[..idx], n + 1);
    }
    format!("{title} (fork #1)")
}

/// P0e: forward bus frames to the plugin sidecar's `event` hook (v1
/// plugin/index.ts:255-259). Lagged frames are skipped (upstream is
/// fire-and-forget); a closed bus ends the pump. Never blocks publishers.
pub fn start_plugin_event_pump(st: &Arc<AppState>) {
    // Subscribe SYNCHRONOUSLY here: last_seq is captured at call time, so
    // frames published between call and first recv are buffered (drain), not
    // skipped. (Spawning first raced: sync route handlers can publish before
    // the task ever ran — caught by the pump test.)
    let mut rx = st.bus.subscribe();
    let st = st.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame_str) => {
                    let Some(plug) = st.plugins.get() else {
                        continue; // sidecar not loaded — frames still on SSE
                    };
                    let Ok(v) = serde_json::from_str::<Value>(&frame_str) else {
                        continue;
                    };
                    let Some(payload) = v.get("payload").cloned() else {
                        continue;
                    };
                    let mut guard = plug.lock().await;
                    if let Err(e) = guard.emit_event(payload).await {
                        tracing::debug!("plugin event pump: {e:#}");
                    }
                }
                Err(ocserve_core::event::RecvError::Lagged(n)) => {
                    tracing::warn!("plugin event pump lagged {n} frames (skipped)");
                }
                Err(ocserve_core::event::RecvError::Closed) => break,
            }
        }
    });
}

/// POST /session — create (wire shape from captured fixture: cost, directory,
/// id, path, projectID, slug, time, title, tokens, version).
async fn post_session(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let id = ocserve_core::ids::ses_id();
    let now = now_ms();
    let worktree = st.paths["worktree"].as_str().unwrap_or("/").to_string();
    let title = body
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    // K-TITLE: freeze default when the client sends none/empty — an empty
    // title renders as "untitled" everywhere (upstream: parentTitlePrefix).
    let title = if title.trim().is_empty() {
        default_session_title(now)
    } else {
        title
    };
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
    ocserve_store::insert_session(&st.writer, &info).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    st.bus.publish(ocserve_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "session.created",
        json!({"sessionID": id, "info": info}),
    ));
    Ok(Json(info))
}

/// POST /session/{id}/fork — freeze session.fork (groups/session.ts:248,
/// session.ts:691). Effect decodes the payload BEFORE the handler runs, so
/// payload errors precede the 404: bad JSON / non-object / non-string
/// `messageID` → 400 `{"_tag":"BadRequest"}`; empty or whitespace body is
/// the NoContent arm (copy all). Returns the NEW Session.Info; messages are
/// copied strictly before `messageID` (freeze `slice(0, findIndex)`; unknown
/// id → findIndex -1 → copy all).
async fn post_fork(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, HttpError> {
    let upto: Option<String> = {
        let text = String::from_utf8_lossy(&body);
        if text.trim().is_empty() {
            None // NoContent arm
        } else {
            let v: Value = serde_json::from_str(&text).map_err(|_| tagged_bad_request())?;
            if !v.is_object() {
                return Err(tagged_bad_request()); // [] / "x" / 42 / null
            }
            match v.get("messageID") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(tagged_bad_request()), // non-string / null
            }
        }
    };
    let src_id = id.clone();
    let src = run_blocking(&st.db, move |db| {
        ocserve_store::load_session_wire(db, &src_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?
    .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))?;

    let new_id = ocserve_core::ids::ses_id();
    let now = now_ms();
    let worktree = st.paths["worktree"].as_str().unwrap_or("/").to_string();
    // createNext shape (session.ts): fresh id/slug, FORKED title, cost/tokens
    // zeroed (fork does not inherit usage), request-ctx directory/path —
    // workspaceID/metadata are not modeled by ocserve's schema (pre-existing).
    let info = json!({
        "id": new_id,
        "projectID": "global",
        "directory": worktree,
        "path": worktree.trim_start_matches('/'),
        "slug": slug_for(&new_id),
        "title": forked_title(src["title"].as_str().unwrap_or("")),
        "version": FREEZE_VERSION,
        "time": {"created": now, "updated": now},
        "cost": 0,
        "tokens": {"input": 0, "output": 0, "reasoning": 0,
                   "cache": {"read": 0, "write": 0}},
    });
    // PERF-10X F1: fork copies the whole source session (32k msgs measured)
    // — inline it parked a worker for the entire copy.
    let writer = st.writer.clone();
    let blobs = st.blobs.clone();
    let db = st.db.clone();
    let fork_info = info.clone();
    let fork_id = id.clone();
    let fork_upto = upto.clone();
    let stats = run_blocking(&db, move |db| {
        let env = ocserve_store::fork::ForkEnv {
            writer: &writer,
            blobs: Some(&*blobs),
            db,
        };
        ocserve_store::fork_session(
            &env,
            &fork_info,
            &fork_id,
            fork_upto.as_deref(),
            ocserve_core::ids::msg_id,
            ocserve_core::ids::prt_id,
        )
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    ocserve_metrics::counter("ocserve_session_fork_total", 1);
    tracing::info!(
        fork = %new_id,
        source = %id,
        upto = ?upto,
        messages = stats.messages,
        parts = stats.parts,
        "session forked"
    );
    // session.created AFTER the copy (D-FORK-1): upstream publishes at
    // createNext BEFORE cloning — a client reacting to the event can read a
    // partial session; ours fires when the history is complete. Every known
    // consumer (dialog-fork, ACP, oc-remote) REST-loads after this response.
    st.bus.publish(ocserve_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "session.created",
        json!({"sessionID": new_id, "info": info}),
    ));
    Ok(Json(info))
}

/// GET /session/{id}/message — [{info, parts}] (captured wrapper shape).
async fn get_messages(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, HttpError> {
    // Freeze rule order (handlers/session.ts:110-117 + observed matrix in
    // message_page_contract.json): query-shape errors FIRST (Effect decodes
    // the whole query before the handler), then before-rules, then session.
    let limit: Option<u64> = match q.get("limit") {
        None => None,
        Some(raw) if raw.is_empty() => None, // `limit=` observed as full history
        Some(raw) => Some(parse_limit(raw).map_err(|message| HttpError::Query { message })?),
    };
    let before: Option<(String, i64)> = match (q.get("before").map(String::as_str), limit) {
        (Some(_), None) => return Err(tagged_bad_request()),
        (Some(b), Some(_)) => {
            Some(ocserve_store::decode_cursor(b).map_err(|_| tagged_bad_request())?)
        }
        (None, _) => None,
    };
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")).into());
    }
    // limit>0 → tuple-ordered cursor page (upstream MessageV2.page); absent/0
    // → full history. Headers ONLY when older messages remain (their rule).
    let mut page_n: Option<u64> = None;
    let mut page_next: Option<String> = None;
    let walk: ocserve_store::MessageWalk = match limit.filter(|n| *n > 0) {
        Some(n) => {
            let page_id = id.clone();
            let before_owned = before.clone();
            let (rows, _more, next) = run_blocking(&st.db, move |db| {
                ocserve_store::page_messages(
                    db,
                    &page_id,
                    n,
                    before_owned.as_ref().map(|(b, t)| (b.as_str(), *t)),
                )
            })
            .await
            .map_err(|e| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                name: "InternalError",
                message: format!("{e:#}"),
            })?;
            page_n = Some(n);
            page_next = next;
            ocserve_store::MessageWalk::Window(rows)
        }
        None => ocserve_store::MessageWalk::Seq { limit: None },
    };
    // F9: memoized paged body. Key carries db path (cross-test/process
    // isolation: same session id in another tmpdir must not collide) +
    // limit + cursor + the live write epoch (checked inside get).
    if page_n.is_some() && page_memo_enabled() {
        let memo_key = format!("{}\u{1}{id}\u{1}{limit:?}\u{1}{before:?}", st.db.display());
        let e0 = ocserve_store::write_epoch();
        {
            let mut m = page_memo().lock();
            if let Some(e) = m.get(&memo_key, e0) {
                drop(m);
                PAGE_MEMO_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return page_response(
                    &headers,
                    &uri,
                    &page_n,
                    &page_next,
                    axum::body::Body::from(e.body),
                )
                .map_err(Into::into);
            }
        }
        PAGE_MEMO_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // assemble the page bounded — oversize aborts the collect and we
        // fall through to the original streaming path (walk not consumed:
        // the assembler gets a clone).
        let walk_asm = walk.clone();
        let db_asm = st.db.clone();
        let sid_asm = id.clone();
        let asm: anyhow::Result<std::result::Result<Vec<u8>, ()>> =
            run_blocking(&db_asm, move |db| {
                let mut buf: Vec<u8> = Vec::new();
                let mut total = 0usize;
                let mut first = true;
                let r = ocserve_store::for_each_message_json(db, &sid_asm, walk_asm, |chunk| {
                    let framed = if first {
                        first = false;
                        format!("[{chunk}")
                    } else {
                        format!(",{chunk}")
                    };
                    if total + framed.len() > PAGE_MEMO_ENTRY_CAP {
                        return Err(anyhow::anyhow!("page memo entry cap"));
                    }
                    total += framed.len();
                    buf.extend_from_slice(framed.as_bytes());
                    Ok(())
                });
                match r {
                    Ok(()) => {
                        // mirror the streaming close exactly: zero chunks
                        // => "[]" (a bare "]" would not be valid JSON)
                        if first {
                            buf.extend_from_slice(b"[]");
                        } else {
                            buf.push(b']');
                        }
                        Ok(Ok(buf))
                    }
                    Err(e) if e.to_string().contains("entry cap") => Ok(Err(())),
                    Err(e) => Err(e),
                }
            })
            .await;
        if let Ok(Ok(buf)) = asm {
            let bytes = bytes::Bytes::from(buf);
            let entry = PageEntry {
                body: bytes.clone(),
                next: page_next.clone(),
            };
            page_memo().lock().put(memo_key, entry, e0);
            return page_response(
                &headers,
                &uri,
                &page_n,
                &page_next,
                axum::body::Body::from(bytes),
            )
            .map_err(Into::into);
        }
        // oversize or assemble error → original streaming path below
    }
    // STREAMED response: one message per chunk through a bounded channel —
    // materializing a 16k-message session as Values OOM-killed the cgroup
    // (93MB response ≈ 400MB+ parsed; AGENTS §2.3 violation caught live).
    // Peak = channel(4) chunks + the message being built.
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
    let db = st.db.clone();
    let sid = id.clone();
    // F4: the tokio blocking pool, not a per-request OS thread — every
    // message_page request used to spawn a fresh thread (spawn+1MB VA per
    // page under load). The producer only does sync store work + channel
    // sends; dropping the JoinHandle keeps it detached exactly like before,
    // and the pool caps concurrent producers (bounded-by-construction).
    tokio::task::spawn_blocking(move || {
        let mut first = true;
        let r = ocserve_store::for_each_message_json(&db, &sid, walk, |chunk| {
            let framed = if first {
                first = false;
                format!("[{chunk}")
            } else {
                format!(",{chunk}")
            };
            tx.blocking_send(framed)
                .map_err(|_| anyhow::anyhow!("client disconnected"))
        });
        if let Err(e) = r {
            tracing::error!("message stream aborted session={sid}: {e:#}");
        }
        // best-effort close bracket (valid JSON even after a mid-stream error)
        let _ = tx.blocking_send(if first {
            "[]".to_string()
        } else {
            "]".to_string()
        });
    });
    let body_stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| {
            (
                Ok::<axum::body::Bytes, std::convert::Infallible>(axum::body::Bytes::from(chunk)),
                rx,
            )
        })
    });
    page_response(
        &headers,
        &uri,
        &page_n,
        &page_next,
        axum::body::Body::from_stream(body_stream),
    )
    .map_err(Into::into)
}

/// PATCH /config + PATCH /global/config — W4 merge (v1 ConfigHttpApi.update:
/// deep-merges the payload, marks the instance for disposal, returns the
/// ECHOED payload — handlers/config.ts:18-21). Atomic file write (tmp in
/// the same dir → rename, .bak kept), then the boot-injected reloader
/// swaps derived payloads so the next GET serves fresh config. Divergence:
/// ocserve treats global==user config (single file); runtime endpoint
/// registries refresh on restart (TESTING §1.6).
async fn patch_config(
    State(st): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let path = config_write_path();
    let mut base: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("read {}: {e}", path.display()),
        })?;
        serde_json::from_str(&raw).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("parse {}: {e}", path.display()),
        })?
    } else {
        json!({})
    };
    deep_merge(&mut base, payload.clone());
    let dir = path.parent().ok_or_else(|| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: "config path has no parent".into(),
    })?;
    std::fs::create_dir_all(dir).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("mkdir {}: {e}", dir.display()),
    })?;
    let tmp = dir.join(format!(".opencode.json.tmp-{}", std::process::id()));
    let data = serde_json::to_vec_pretty(&base).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("serialize: {e}"),
    })?;
    std::fs::write(&tmp, &data).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("write {}: {e}", tmp.display()),
    })?;
    if path.exists() {
        let _ = std::fs::copy(&path, dir.join(".opencode.json.bak"));
    }
    std::fs::rename(&tmp, &path).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("rename {}: {e}", path.display()),
    })?;
    // instance disposal analog: rebuild derived payloads + registry AND
    // reconcile MCP (a PATCH that adds a server now connects it — H1
    // unifies every reload path through watch::reconcile)
    if let Err(e) = watch::reconcile(&st).await {
        tracing::warn!("config reload failed (restart to apply): {e:#}");
    }
    tracing::info!("config patched: {}", path.display());
    Ok(Json(payload))
}

/// POST /session/{id}/summarize — W3 compaction-lite (v1 handlers/session.ts
/// summarize: payload {providerID, modelID, auto?} → true). Runs ONE
/// transient text-only turn carrying buildPrompt's instruction + serialized
/// history (ocserve-core::compact); the summary lands as an ordinary
/// assistant message (divergence: no compaction-state/history filtering —
/// TESTING §1.6). Synchronous like upstream (`yield* promptSvc.loop`).
async fn post_summarize(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(sid): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let exists_id = sid.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {sid}")));
    }
    let provider = payload
        .get("providerID")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let model_id = payload
        .get("modelID")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let (Some(provider), Some(model_id)) = (provider, model_id) else {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "providerID and modelID are required".into(),
        });
    };

    let release = lock_session(&st, &sid).await?;
    let Ok(_guard) = release.arc().try_lock() else {
        return Err(session_busy(&sid));
    };

    // C5 retrofit (M6, COMPACTION.md §3): manual summarize = v1 source
    // (handlers/session.ts:273-293 at the FREEZE TAG v1.18.31, verified):
    // compactSvc.create(anchor with compaction part) + loop → pending task
    // → engine process → summary-exit. The run's outer machinery detects
    // the pending anchor — no generation runs (exit via last-message
    // summary check). Session-shaped history load ONLY for findLast user's
    // agent (v1 line 280).
    let history = run_blocking(&st.db, {
        let sid = sid.clone();
        move |db| ocserve_store::load_messages(db, &sid, None) // allow:load_messages (summarize compaction)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    let agent = history
        .iter()
        .rev()
        .find(|(info, _)| info["role"] == "user")
        .and_then(|(info, _)| info["agent"].as_str())
        .unwrap_or("build")
        .to_string();
    let prompt_payload = json!({
        "model": {"providerID": provider, "modelID": model_id},
        "agent": agent,
        "parts": [],
    });
    let ctx = build_prompt_context(&st, &prompt_payload, &sid)?;
    let writer = st.writer.clone();
    ocserve_core::compaction::persist_anchor(&ctx, &writer, &sid, &agent, false, false)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?;
    ocserve_core::prompt::run_prompt_with(
        &ctx,
        &writer,
        &sid,
        &prompt_payload,
        ocserve_core::prompt::RunOpts {
            // the ANCHOR is already the newest message — nothing from the
            // payload persists; pending→engine→summary-exit short-circuits
            // before any generation (persist_user=false keeps chat.message
            // silent, P0c invariant)
            persist_user: false,
            skip_history: false,
            prelude: None,
            tools_enabled: false,
            ..Default::default()
        },
    )
    .await
    .map_err(prompt_err)?;
    // P0e: session.compacted (magic-context consumes it after a summary)
    st.bus.publish(ocserve_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "session.compacted",
        json!({"sessionID": sid}),
    ));
    Ok(Json(json!(true)))
}

/// W5 admin bundle — POST /mcp/{name}/connect|disconnect (the client's
/// generic `/$action` path is identical to v1 McpPaths.connect/disconnect),
/// DELETE /mcp/{name}/auth, PUT/DELETE /auth/{providerID} (ocserve-OWN auth
/// overlay — NEVER the legacy auth.json ocserve reads), GET /provider/auth,
/// POST /global/dispose.
fn mcp_not_found(name: &str) -> HttpError {
    HttpError::TaggedData {
        status: StatusCode::NOT_FOUND,
        tag: "McpServerNotFoundError",
        fields: json!({"name": name, "message": format!("MCP server not found: {name}")}),
    }
}

async fn mcp_action(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((name, action)): axum::extract::Path<(String, String)>,
) -> Result<Json<Value>, HttpError> {
    let hub = st.mcp.get().cloned().ok_or_else(|| HttpError::TaggedData {
        status: StatusCode::SERVICE_UNAVAILABLE,
        tag: "McpUnavailable",
        fields: json!({"message": "MCP hub not initialized"}),
    })?;
    let res = match action.as_str() {
        "connect" => hub.connect(&name).await,
        "disconnect" => hub.disconnect(&name).await,
        _ => Err(anyhow::anyhow!("unknown MCP action: {action}")),
    };
    res.map_err(|e| {
        if e.to_string().contains("not found") {
            mcp_not_found(&name)
        } else {
            HttpError::Api(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                name: "InternalError",
                message: format!("{e:#}"),
            })
        }
    })?;
    Ok(Json(json!(true)))
}

async fn mcp_auth_remove(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<Value>, HttpError> {
    // v1 authRemove: existence check first, then removeAuth → {success:true}
    let known = st
        .mcp
        .get()
        .map(|h| {
            let m = h.statuses();
            m.as_object()
                .map(|o| o.contains_key(&name))
                .unwrap_or(false)
                || h.known(&name)
        })
        .unwrap_or(false);
    if !known {
        return Err(mcp_not_found(&name));
    }
    // no OAuth MCP servers are configured → removing stored auth is a no-op
    // success (divergence TESTING §1.6)
    Ok(Json(json!({"success": true})))
}

/// ocserve-owned auth overlay — the file ocserve WRITES; the legacy auth.json
/// ocserve READS stays untouched (a bad write there would brick the TUI).
fn auth_overlay_path(st: &AppState) -> std::path::PathBuf {
    st.db
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("auth-overlay.json")
}

fn write_json_atomic(path: &std::path::Path, value: &Value) -> Result<(), ApiError> {
    let dir = path.parent().ok_or_else(|| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: "path has no parent".into(),
    })?;
    std::fs::create_dir_all(dir).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("mkdir: {e}"),
    })?;
    let tmp = dir.join(format!(".tmp-{}-auth", std::process::id()));
    let data = serde_json::to_vec_pretty(value).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("serialize: {e}"),
    })?;
    std::fs::write(&tmp, data).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("write: {e}"),
    })?;
    std::fs::rename(&tmp, path).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("rename: {e}"),
    })?;
    Ok(())
}

async fn auth_put(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(pid): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if !body.is_object() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "auth payload must be an object".into(),
        });
    }
    let path = auth_overlay_path(&st);
    let mut overlay: Value = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };
    overlay[&pid] = body; // Auth.Info union stored verbatim (discriminator "type")
    write_json_atomic(&path, &overlay)?;
    if let Err(e) = watch::reconcile(&st).await {
        tracing::warn!("payload reload failed: {e:#}");
    }
    tracing::info!("auth overlay set provider={pid}");
    Ok(Json(json!(true)))
}

async fn auth_delete(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(pid): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = auth_overlay_path(&st);
    if path.exists() {
        let mut overlay: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = overlay.as_object_mut() {
            obj.remove(&pid);
        }
        write_json_atomic(&path, &overlay)?;
        if let Err(e) = watch::reconcile(&st).await {
            tracing::warn!("payload reload failed: {e:#}");
        }
    }
    tracing::info!("auth overlay remove provider={pid}");
    Ok(Json(json!(true)))
}

/// GET /provider/auth — {providerID: [{type,label}]}. v1 derives these from
/// auth HOOKS (provider/auth.ts:131); ocserve serves what ocserve can do —
/// api-key methods for every configured provider (divergence TESTING §1.6;
/// matches the PUT /auth {type:"api"} client flow).
async fn provider_auth_methods(State(st): State<Arc<AppState>>) -> Json<Value> {
    let payloads = st.payloads.read();
    let mut out = serde_json::Map::new();
    // LIVE shape: config_providers.providers is a LIST of {id, ...} — the
    // dict assumption returned {} (caught on the W5 live battery)
    if let Some(list) = payloads
        .config_providers
        .get("providers")
        .and_then(|p| p.as_array())
    {
        for e in list {
            if let Some(pid) = e.get("id").and_then(|v| v.as_str()) {
                out.insert(
                    pid.to_string(),
                    json!([{"type": "api", "label": "API Key"}]),
                );
            }
        }
    }
    Json(serde_json::Value::Object(out))
}

/// POST /global/dispose — v1 (global.ts:84): dispose + emit + true. Ocserve
/// analog: durable+live `server.instance.disposed` (EventReducer clears
/// client state) + payload reload. Divergence: no instance registry
/// (TESTING §1.6).
async fn global_dispose(State(st): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    ocserve_store::append_event(&st.writer, None, "server.instance.disposed", &json!({})).map_err(
        |e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        },
    )?;
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    st.bus.publish(ocserve_core::event::frame(
        &dir,
        "server.instance.disposed",
        json!({}),
    ));
    if let Err(e) = watch::reconcile(&st).await {
        tracing::warn!("payload reload failed: {e:#}");
    }
    tracing::info!("global dispose: emitted + payloads reloaded");
    Ok(Json(json!(true)))
}

/// POST /session/{id}/revert — v1 reverts to {messageID, partID?}: the
/// session info carries a `revert` marker; the message list is sliced from
/// the target (synchronous slice — v1 removes rows lazily via `cleanup`
/// before the next prompt; ocserve marks only, so history survives until
/// the next prompt's filter step). Snapshot restore is engine-out
/// (D-REVERT-NOSNAP): `snapshot`/`diff` fields are not populated.
async fn post_revert(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<axum::response::Response, ApiError> {
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let message_id = payload
        .get("messageID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if message_id.is_empty() {
        // freeze shape: {"name":"BadRequest","data":{"message":...,
        // "kind":"Payload"}} — probed 2026-10-08.
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "name": "BadRequest",
                "data": {
                    "message": "Missing key\n  at [\"messageID\"]",
                    "kind": "Payload"
                }
            })),
        )
            .into_response());
    }
    let part_id = payload.get("partID").cloned();
    let mut marker = json!({"messageID": message_id});
    if let Some(p) = part_id
        && !p.is_null()
    {
        marker["partID"] = p;
    }
    ocserve_store::set_session_revert(&st.writer, &id, Some(&marker)).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    st.bus.publish(ocserve_core::event::frame(
        &dir,
        "session.updated",
        json!({"sessionID": id, "info": {"id": id, "revert": marker}}),
    ));
    let sid = id.clone();
    run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .map(|j| axum::Json(j).into_response())
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// POST /session/{id}/unrevert — clears the marker (v1 clearRevert).
async fn post_unrevert(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    ocserve_store::set_session_revert(&st.writer, &id, None).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();
    st.bus.publish(ocserve_core::event::frame(
        &dir,
        "session.updated",
        json!({"sessionID": id, "info": {"id": id, "revert": Value::Null}}),
    ));
    let sid = id.clone();
    run_blocking(&st.db, move |db| ocserve_store::load_session_wire(db, &sid))
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

/// POST /instance/dispose — freeze returns `true` (marks the instance for
/// disposal; a single-instance server's equivalent is publishing
/// server.instance.disposed like /global/dispose).
async fn instance_dispose(State(st): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let _disposed = global_dispose(State(st)).await?;
    Ok(Json(json!(true)))
}

/// POST /session/{id}/init — runs the built-in `init` command
/// (command/index.ts Default.INIT, template initialize.txt) with the
/// caller's model. Freeze payload {messageID, providerID, modelID}; missing
/// model fields → 400 Effect Payload envelope ("Missing key at [modelID]").
async fn post_init(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<axum::response::Response, ApiError> {
    let provider = payload.get("providerID").and_then(|v| v.as_str());
    let model = payload.get("modelID").and_then(|v| v.as_str());
    let (Some(provider), Some(model)) = (provider, model) else {
        let missing = if provider.is_none() {
            "providerID"
        } else {
            "modelID"
        };
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "name": "BadRequest",
                "data": {
                    "message": format!("Missing key\n  at [\"{missing}\"]"),
                    "kind": "Payload"
                }
            })),
        )
            .into_response());
    };
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    // Reuse the /command path: command=init, arguments="", model from payload.
    let cmd_payload = json!({
        "command": "init",
        "arguments": "",
        "model": format!("{provider}/{model}"),
        "messageID": payload.get("messageID").cloned().unwrap_or(Value::Null),
    });
    post_command(State(st), axum::extract::Path(id), Json(cmd_payload))
        .await
        .map(|j| j.into_response())
}

/// POST /provider/{id}/oauth/authorize — ocserve has no OAuth flows; the
/// freeze shape for a non-OAuth provider is 400 {"name":"BadRequest",
/// "data":{}} (probed). OAuth login remains a named deferral (PLAN §17).
async fn post_provider_oauth(
    axum::extract::Path(_id): axum::extract::Path<String>,
) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"name": "BadRequest", "data": {}})),
    )
        .into_response()
}

/// POST /provider/{id}/oauth/callback — same divergence as authorize.
async fn post_provider_oauth_callback(
    axum::extract::Path(_id): axum::extract::Path<String>,
) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"name": "BadRequest", "data": {}})),
    )
        .into_response()
}

/// POST /mcp/{name}/auth/authenticate — unknown → 404 tagged; known (all
/// non-OAuth in practice) → 400 McpUnsupportedOAuthError (probe shapes).
async fn post_mcp_auth_authenticate(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<axum::response::Response, HttpError> {
    mcp_auth_gate(&st, &name)?;
    Err(HttpError::TaggedData {
        status: StatusCode::BAD_REQUEST,
        tag: "McpUnsupportedOAuthError",
        fields: json!({"error": format!("MCP server {name} does not support OAuth")}),
    })
}

/// POST /mcp/{name}/auth/callback — same gate as authenticate.
async fn post_mcp_auth_callback(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<axum::response::Response, HttpError> {
    mcp_auth_gate(&st, &name)?;
    Err(HttpError::TaggedData {
        status: StatusCode::BAD_REQUEST,
        tag: "McpUnsupportedOAuthError",
        fields: json!({"error": format!("MCP server {name} does not support OAuth")}),
    })
}

/// Shared existence check: unknown server → tagged 404 (probed shape).
fn mcp_auth_gate(st: &Arc<AppState>, name: &str) -> Result<(), HttpError> {
    let known = st
        .mcp
        .get()
        .map(|h| h.statuses().get(name).is_some())
        .unwrap_or(false);
    if known {
        return Ok(());
    }
    Err(HttpError::TaggedData {
        status: StatusCode::NOT_FOUND,
        tag: "McpServerNotFoundError",
        fields: json!({
            "name": name,
            "message": format!("MCP server not found: {name}"),
        }),
    })
}

/// GET /session/{id}/message/{mid} → {info, parts}; unknown → 404
/// NotFoundError envelope (probed freeze: {"name":"NotFoundError",
/// "data":{"message":"Message not found: <mid>"}}).
async fn get_message_by_id(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((id, mid)): axum::extract::Path<(String, String)>,
) -> Result<axum::response::Response, ApiError> {
    let db = st.db.clone();
    let sid = id.clone();
    let mid_c = mid.clone();
    let found = run_blocking(&db, move |db| {
        ocserve_store::message_by_id(db, &sid, &mid_c)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    match found {
        Some(v) => Ok(axum::Json(v).into_response()),
        None => Err(ApiError::not_found(format!("Message not found: {mid}"))),
    }
}

/// POST /session/{id}/permissions/{pid} — deprecated v1 respond route; maps
/// onto the same gate as /permission/{id}/reply. Body {response: once|always|
/// reject}; unknown id → tagged PermissionNotFoundError (probed).
async fn post_session_permission_respond(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((_id, pid)): axum::extract::Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<axum::response::Response, HttpError> {
    let reply = body
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or("reject")
        .to_string();
    if !st.gate.reply(&pid, &reply) {
        return Err(HttpError::TaggedData {
            status: StatusCode::NOT_FOUND,
            tag: "PermissionNotFoundError",
            fields: json!({
                "requestID": pid,
                "message": format!("Permission request not found: {pid}"),
            }),
        });
    }
    Ok(axum::Json(json!(true)).into_response())
}

/// POST /log — v1 control-plane log writer; body {service,level,message,extra?}
/// → true (probed). Writes to the tracing target so it lands in the journal.
async fn post_log(Json(body): Json<Value>) -> Result<Json<Value>, HttpError> {
    let level = body.get("level").and_then(|v| v.as_str()).unwrap_or("info");
    let service = body
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("app");
    let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("");
    match level {
        "debug" => tracing::debug!(target: "ocserve::client", service, "{message}"),
        "warn" => tracing::warn!(target: "ocserve::client", service, "{message}"),
        "error" => tracing::error!(target: "ocserve::client", service, "{message}"),
        _ => tracing::info!(target: "ocserve::client", service, "{message}"),
    }
    Ok(Json(json!(true)))
}

/// POST /session/search — W1 content search over part payloads (contract
/// block PLAN §17). ≥3 chars → trigram FTS substring MATCH; shorter → LIKE
/// fallback (same projection, one query either way). Inline AND blobbed
/// parts covered (projection stores uncompressed text).
async fn search_messages(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let query = body
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() || query.chars().count() > 512 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "query must be a non-empty string of at most 512 characters".into(),
        });
    }
    let scope = body
        .get("sessionID")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let limit = body
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 200) as u32;
    let offset = body.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let needle = query.clone();
    let (hits, truncated) = run_blocking(&st.db, move |db| {
        ocserve_store::search_parts(db, &needle, scope.as_deref(), limit, offset)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    let out = json!({
        "hits": hits
            .iter()
            .map(|h| json!({
                "sessionID": h.session_id,
                "messageID": h.message_id,
                "partID": h.part_id,
                "role": h.role,
                "time": h.time,
                "snippet": ocserve_store::snippet(&h.text, &query),
            }))
            .collect::<Vec<_>>(),
        "truncated": truncated,
    });
    Ok(Json(out))
}

/// POST /session/{id}/message — sync prompt (blocks until stream completes,
/// returns {info, parts}; captured in testdata/m2/prompt_response.json).
/// v1 currentModel(sessionID): payload.model → session's stored model
/// (updated at prompt time) → configured default.
fn resolve_model(st: &Arc<AppState>, payload: &Value, session_id: &str) -> (String, String) {
    if let (Some(p), Some(m)) = (
        payload
            .pointer("/model/providerID")
            .and_then(|v| v.as_str()),
        payload.pointer("/model/modelID").and_then(|v| v.as_str()),
    ) {
        return (p.to_string(), m.to_string());
    }
    if let Ok(conn) = ocserve_store::pragma::open_reader(&st.db) {
        let stored: Option<String> = conn
            .query_row(
                "SELECT model FROM session WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        if let Some(raw) = stored
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            let p = v.get("providerID").and_then(|x| x.as_str()).unwrap_or("");
            let m = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            if !p.is_empty() && !m.is_empty() {
                return (p.to_string(), m.to_string());
            }
        }
    }
    st.llm.read().default_model.clone()
}

/// Resolve agent/model/system/endpoint/rules into a runnable prompt context.
/// Shared by POST /message (sync) and POST /prompt_async (backgrounded).
fn build_prompt_context(
    st: &Arc<AppState>,
    payload: &Value,
    session_id: &str,
) -> Result<ocserve_core::prompt::PromptContext, ApiError> {
    let agent = payload
        .get("agent")
        .and_then(|a| a.as_str())
        .filter(|a| !a.is_empty())
        .map(String::from)
        .unwrap_or_else(|| st.llm.read().default_agent.clone());
    let system = st
        .llm
        .read()
        .systems
        .get(&agent)
        .cloned()
        .unwrap_or_else(|| crate::BUILD_SYSTEM_BLURB.to_string());
    let (pid, mid) = resolve_model(st, payload, session_id);
    let (base_url, api_key) =
        st.llm
            .read()
            .endpoints
            .get(&pid)
            .cloned()
            .ok_or_else(|| ApiError {
                status: StatusCode::BAD_REQUEST,
                name: "BadRequest",
                message: format!("no endpoint configured for provider {pid}"),
            })?;
    let pricing = st
        .llm
        .read()
        .pricing
        .get(&(pid.clone(), mid.clone()))
        .copied();
    // agent permission rules (v1 wire shape → evaluator)
    let rules: Vec<ocserve_tools::Rule> = st
        .payloads
        .read()
        .agent
        .iter()
        .find(|a| a["name"].as_str() == Some(agent.as_str()))
        .and_then(|a| a["permission"].as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(ocserve_tools::Rule {
                        permission: r["permission"].as_str()?.to_string(),
                        pattern: r["pattern"].as_str()?.to_string(),
                        action: r["action"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let model_limit = st
        .llm
        .read()
        .limits
        .get(&(pid.clone(), mid.clone()))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let compaction = {
        let payloads = st.payloads.read();
        ocserve_core::compact::CompactionCfg::from_value(Some(&payloads.compaction))
    };
    let compaction_system = st
        .llm
        .read()
        .systems
        .get("compaction")
        .cloned()
        .unwrap_or_else(|| system.clone());
    let ctx = ocserve_core::prompt::PromptContext {
        db: st.db.clone(),
        blobs: st.blobs.clone(),
        bus: st.bus.clone(),
        directory: st.paths["directory"].as_str().unwrap_or("/").to_string(),
        agent: agent.clone(),
        system,
        endpoint: ocserve_core::prompt::LlmEndpoint {
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
        questions: st.question_gate.clone(),
        model_limit,
        max_rounds: st.max_rounds,
        cost_ceiling: st.cost_ceiling,
        compaction,
        compaction_system,
    };
    Ok(ctx)
}

/// Per-session prompt lock (insert-only while active; bounded by in-flight).
/// JS-Number-style rendering for Effect query error messages (JS prints
/// ≥1e21 in exponent form WITH a plus sign: "1e+21"; Rust's Display does
/// not — mirror observed upstream bytes).
fn fmt_js(v: f64) -> String {
    if v.is_finite() && v.abs() >= 1e21 {
        let raw = format!("{v:e}"); // e.g. 1e21 / -1.5e21
        if let Some(pos) = raw.find('e') {
            return format!("{}e+{}", &raw[..pos], &raw[pos + 1..]);
        }
        raw
    } else {
        format!("{v}")
    }
}

/// `?limit=` validation — freeze-probed matrix (message_page_contract.json):
/// absent/empty/0 → full history; integers (incl. "+5", "1e3") accepted up
/// to 2^53-1; negatives → range message; non-integers → integer message
/// (junk → "NaN"); mirror Effect's NumberFromString + Int + nonnegative
/// checks in observed order (integer-ness first, then ≥0). Rust's f64 parse
/// diverges from JS Number only on hex/whitespace forms — clients never
/// send those (documented, corpus note).
fn parse_limit(raw: &str) -> Result<u64, String> {
    const INT_MSG: &str = "Expected an integer, got {}\n  at [\"limit\"]";
    match raw.parse::<f64>() {
        Err(_) => Err(INT_MSG.replace("{}", "NaN")),
        Ok(v) if v.is_nan() => Err(INT_MSG.replace("{}", "NaN")),
        Ok(v) if v.is_infinite() => {
            Err(INT_MSG.replace("{}", if v > 0.0 { "Infinity" } else { "-Infinity" }))
        }
        Ok(v) if v.trunc() != v => Err(INT_MSG.replace("{}", &fmt_js(v))),
        Ok(v) if v.abs() > 9007199254740991.0 => Err(INT_MSG.replace("{}", &fmt_js(v))),
        Ok(v) if v < 0.0 => Err(format!(
            "Expected a value greater than or equal to 0, got {}\n  at [\"limit\"]",
            fmt_js(v)
        )),
        Ok(v) => Ok(v as u64),
    }
}

fn tagged_bad_request() -> HttpError {
    HttpError::Tagged {
        status: StatusCode::BAD_REQUEST,
        tag: "BadRequest",
    }
}

/// Abort-safe cleanup (antagonism A2): remove the prompt_locks entry when the
/// holder's task ends ANY way — normal completion, `?abort`, task panic,
/// delete_session — because post-run cleanup code never executes on abort
/// (entries only self-heal on the next prompt). Holds its own Arc clone so
/// the strong_count ≤ 2 check is exact during Drop. Concurrent holders or
/// waiters push the count above 2 → skip (entry stays — it's live).
struct LockRelease {
    locks: std::sync::Arc<
        parking_lot::Mutex<
            std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
        >,
    >,
    sid: String,
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl LockRelease {
    fn arc(&self) -> &std::sync::Arc<tokio::sync::Mutex<()>> {
        &self.lock
    }
}

impl Drop for LockRelease {
    fn drop(&mut self) {
        // EXACT reference accounting (the first design had callers holding a
        // separate Arc clone → drop-order dependent skip, caught by the abort
        // test): ALL locking goes through arc() so the only refs are the map
        // entry + this release. remove iff count ≤ 2 (map + self).
        let mut map = self.locks.lock();
        if let Some(arc) = map.get(&self.sid)
            && std::sync::Arc::strong_count(arc) <= 2
        {
            map.remove(&self.sid);
        }
    }
}

/// Session prompt lock + abort-safe cleanup in ONE object: callers lock via
/// `release.arc()` (never a second Arc clone) so Drop's count is exact, and
/// dropping release — normal end, `?`, panic, or task abort — evicts the map
/// entry. Concurrent holders/waiters each hold their own release (count > 2)
/// so live entries are never evicted.
async fn lock_session(st: &Arc<AppState>, sid: &str) -> Result<LockRelease, ApiError> {
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
    Ok(LockRelease {
        locks: std::sync::Arc::clone(&st.prompt_locks),
        sid: sid.to_string(),
        lock,
    })
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
    let ctx = build_prompt_context(&st, &payload, &id)?;
    let _release = lock_session(&st, &id).await?;
    let _guard = _release.arc().lock().await;
    let writer = st.writer.clone();
    let result = ocserve_core::prompt::run_prompt(&ctx, &writer, &id, &payload)
        .await
        .map_err(prompt_err)?;
    drop(_guard);
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
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let ctx = build_prompt_context(&st, &payload, &id)?;
    // release MOVES INTO the task: the handler returns 204 immediately, so a
    // handler-scoped guard would drop (and evict the lock entry) while the
    // prompt is still running (antagonism A2 — ownership, not just RAII).
    let release = lock_session(&st, &id).await?;
    let writer = st.writer.clone();
    let sid = id.clone();
    let generation = st
        .prompt_gen
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tasks = st.prompt_tasks.clone();
    let tasks_insert = st.prompt_tasks.clone();
    let handle = tokio::spawn(async move {
        let _release = release;
        let guard = _release.arc().lock().await;
        match ocserve_core::prompt::run_prompt(&ctx, &writer, &sid, &payload).await {
            Ok(_) => {}
            Err(e) => tracing::error!("prompt_async failed session={sid}: {e:#}"),
        }
        drop(guard);
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
    run_blocking(&st.db, move |db| ocserve_store::load_children(db, &id))
        .await
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
    run_blocking(&st.db, move |db| ocserve_store::load_todos(db, &id))
        .await
        .map(|v| Json(Value::Array(v)))
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })
}

/// PATCH /session/{id} — oc-remote rename sends {title} only. Other
/// UpdatePayload fields (metadata/permission/time.archived) are not stored
/// by ocserve — accepted and ignored (documented divergence).
async fn patch_session(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let mut dirty = false;
    if let Some(title) = payload.get("title").and_then(|t| t.as_str()) {
        ocserve_store::update_session_title(&st.writer, &id, title).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?;
        dirty = true;
    }
    // K-MODEL-STATE: freeze Patch semantics for model/agent (oc-remote only
    // calls title; TUI/other clients patch model+agent). permission is
    // deliberately NOT patchable (no caller in evidence — ocserve's internal
    // key format differs from upstream's ruleset; divergence noted K row).
    if payload.get("model").is_some() {
        let raw = payload.get("model").cloned().unwrap_or(Value::Null);
        let model_json: Value = match raw {
            Value::Null => Value::Null, // clear → SQL NULL → resolve falls to default
            Value::Object(ref m) => {
                let ok = m
                    .get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| !v.is_empty())
                    && m.get("providerID")
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| !v.is_empty());
                if !ok {
                    return Err(ApiError {
                        status: StatusCode::BAD_REQUEST,
                        name: "BadRequest",
                        message: "model must be {id, providerID, variant?} or null".into(),
                    });
                }
                Value::String(
                    serde_json::to_string(&json!({
                        "id": m["id"],
                        "providerID": m["providerID"],
                        "variant": m.get("variant").cloned().unwrap_or(json!("default")),
                    }))
                    .map_err(|e| ApiError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        name: "InternalError",
                        message: format!("{e:#}"),
                    })?,
                )
            }
            _ => {
                return Err(ApiError {
                    status: StatusCode::BAD_REQUEST,
                    name: "BadRequest",
                    message: "model must be {id, providerID, variant?} or null".into(),
                });
            }
        };
        st.writer
            .write(vec![ocserve_store::WriteOp::Sql {
                sql: "UPDATE session SET model = ?2 WHERE id = ?1".into(),
                params: vec![id.clone().into(), model_json],
            }])
            .map_err(|e| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                name: "InternalError",
                message: format!("{e:#}"),
            })?;
        dirty = true;
    }
    if payload.get("agent").is_some() {
        let agent = match payload.get("agent") {
            Some(Value::Null) => String::new(),
            Some(Value::String(a)) if !a.is_empty() => a.clone(),
            _ => {
                return Err(ApiError {
                    status: StatusCode::BAD_REQUEST,
                    name: "BadRequest",
                    message: "agent must be a non-empty string or null".into(),
                });
            }
        };
        st.writer
            .write(vec![ocserve_store::WriteOp::Sql {
                sql: "UPDATE session SET agent = ?2 WHERE id = ?1".into(),
                params: vec![id.clone().into(), agent.into()],
            }])
            .map_err(|e| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                name: "InternalError",
                message: format!("{e:#}"),
            })?;
        dirty = true;
    }
    let info_id = id.clone();
    let info = run_blocking(&st.db, move |db| {
        ocserve_store::load_session_wire(db, &info_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?
    .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))?;
    if dirty {
        // partial session.updated (prompt's own partial shape) so other
        // clients re-sort/re-render without a refetch
        st.bus.publish(ocserve_core::event::frame(
            st.paths["directory"].as_str().unwrap_or("/"),
            "session.updated",
            json!({"sessionID": id, "info": {"id": id, "title": info["title"],
                                              "model": info["model"], "agent": info["agent"]}}),
        ));
    }
    Ok(Json(info))
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
    let found = ocserve_store::delete_session(&st.writer, &st.db, &id).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })?;
    if !found {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    // P0e: magic-context consumes session.deleted (live publish — the
    // durable rows cascade away with the session).
    st.bus.publish(ocserve_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "session.deleted",
        json!({"sessionID": id}),
    ));
    Ok(Json(json!(true)))
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
    let found =
        ocserve_store::delete_message(&st.writer, &st.db, &sid, &mid).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })?;
    if !found {
        return Err(ApiError::not_found(format!("Message not found: {mid}")));
    }
    // P0e: magic-context consumes message.removed (live publish — durable
    // rows cascade away with the message).
    st.bus.publish(ocserve_core::event::frame(
        st.paths["directory"].as_str().unwrap_or("/"),
        "message.removed",
        json!({"sessionID": sid, "messageID": mid}),
    ));
    Ok(Json(json!(true)))
}

async fn delete_part_route(
    State(st): State<Arc<AppState>>,
    axum::extract::Path((_sid, mid, pid)): axum::extract::Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    ocserve_store::delete_part(&st.writer, &st.db, &mid, &pid)
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
    ocserve_store::update_part(&st.writer, &st.blobs, &sid, &mid, &pid, &part)
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
    // M3 (same class as find_files): `read_dir` + per-entry `metadata()` is
    // blocking syscalls, inline on a tokio worker. A directory with thousands
    // of entries would park the worker for the whole loop.
    let dir_for_worker = dir.clone();
    let raw_owned = raw.to_string();
    let rd = tokio::task::spawn_blocking(move || std::fs::read_dir(&dir_for_worker))
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalServerError",
            message: format!("list worker: {e}"),
        })?
        .map_err(|e| ApiError {
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
        let rel = if raw_owned.is_empty() || raw_owned == "." {
            name.clone()
        } else {
            format!("{}/{}", raw_owned.trim_end_matches('/'), name)
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
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/")).to_path_buf();
    let query = q
        .get("query")
        .map(String::as_str)
        .unwrap_or("")
        .to_lowercase();
    if query.is_empty() {
        return Ok(Json(json!([])));
    }
    let want_type = q.get("type").map(String::as_str).unwrap_or("").to_string();
    let limit: usize = q
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let glob_mode = query.contains('*') || query.contains('?');
    // M3: the walk is a blocking filesystem scan (bounded at 20,000 dirs, but
    // a big worktree still means thousands of `read_dir` syscalls per
    // request). It ran INLINE on a tokio worker — the same convoy class as the
    // pre-F1 SQLite work, which is exactly what `run_blocking` exists for.
    // The load harness never caught this because its LIST_PATH is an empty
    // /tmp dir; on a real worktree it would park a worker.
    let out = tokio::task::spawn_blocking(move || {
        walk_files(&base, &query, &want_type, limit, glob_mode)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalServerError",
        message: format!("find_files worker: {e}"),
    })?
    .map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: format!("find_files: {e:#}"),
    })?;
    Ok(Json(Value::Array(
        out.into_iter().map(Value::String).collect(),
    )))
}

/// Thread the last `walk_files` executed on (M3 test seam).
///
/// Global, not `thread_local!`: the walk runs on a *blocking-pool* thread, so
/// a thread-local record would be written on one thread and read on another —
/// the first version of this seam was invisible for exactly that reason (caught
/// by the test asserting on it).
static WALK_THREAD: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

/// Blocking body of `GET /find/file` — a bounded, single-threaded directory
/// walk. Existing bounds kept exactly (20,000 dirs visited, stop once the
/// result set reaches 4×limit); only the *thread* it runs on changed.
fn walk_files(
    base: &std::path::Path,
    query: &str,
    want_type: &str,
    limit: usize,
    glob_mode: bool,
) -> anyhow::Result<Vec<String>> {
    // M3 test seam: which thread executed the walk. Async tokio workers and
    // the blocking pool are different threads, so comparing this with the
    // thread a request body runs on proves the walk was moved OFF the worker
    // without depending on wall-clock timing (a small tree walks in
    // microseconds — the first version of this test used a ticker and passed
    // even with the walk inline, i.e. it was not discriminating).
    if let Ok(mut g) = WALK_THREAD.lock() {
        *g = Some(std::thread::current().id());
    }
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
                ocserve_tools::wildcard_match(&name, query)
            } else {
                child_rel.to_lowercase().contains(query)
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
        let pa = a.to_lowercase().starts_with(query);
        let pb = b.to_lowercase().starts_with(query);
        pb.cmp(&pa).then_with(|| a.cmp(b))
    });
    out.truncate(limit);
    Ok(out)
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

/// GET /question — all pending question requests (v1 Question.Request list;
/// oc-remote ConnectionService polls this while a dialog may be open).
async fn get_questions(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(Value::Array(st.question_gate.list()))
}

/// POST /question/{id}/reply — {answers: [[labels]]} (v1 Reply payload) → true.
async fn post_question_reply(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let raw = body
        .get("answers")
        .and_then(|a| a.as_array())
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "body must be {answers: [[String]]}".into(),
        })?;
    let mut answers: Vec<Vec<String>> = Vec::with_capacity(raw.len());
    for a in raw {
        let row = a.as_array().ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "each answer must be an array of labels".into(),
        })?;
        answers.push(
            row.iter()
                .map(|l| l.as_str().unwrap_or_default().to_string())
                .collect(),
        );
    }
    if st.question_gate.reply(&id, answers) {
        Ok(Json(json!(true)))
    } else {
        Err(ApiError::not_found(format!("Question not found: {id}")))
    }
}

/// POST /question/{id}/reject — no body (oc-remote posts empty) → true.
async fn post_question_reject(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    if st.question_gate.reject(&id) {
        Ok(Json(json!(true)))
    } else {
        Err(ApiError::not_found(format!("Question not found: {id}")))
    }
}

// ---- oc-remote contract family (Batch 4: command/shell) ----

/// v1 argsRegex tokenizer: quoted strings or non-space runs, then
/// quoteTrimRegex strips one leading/trailing quote per token.
pub fn split_command_args(arguments: &str) -> Vec<String> {
    let bytes = arguments.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let (tok, next) = if c == b'"' || c == b'\'' {
            let quote = c;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            let end = (j + 1).min(bytes.len());
            (
                std::str::from_utf8(&bytes[i..end])
                    .unwrap_or("")
                    .to_string(),
                end,
            )
        } else {
            let mut j = i;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            (
                std::str::from_utf8(&bytes[i..j]).unwrap_or("").to_string(),
                j,
            )
        };
        // quoteTrim: strip one leading then one trailing quote char
        let mut t = tok.as_str();
        if t.starts_with(['"', '\'']) {
            t = &t[1..];
        }
        if t.ends_with(['"', '\'']) && !t.is_empty() {
            t = &t[..t.len() - 1];
        }
        out.push(t.to_string());
        i = next;
    }
    out
}

/// v1 command template expansion (prompt.ts command()): $N placeholders (last
/// position swallows the remaining args), then $ARGUMENTS, then append rule.
/// `!`…`` shell snippets (ConfigMarkdown.shell) are NOT evaluated — neither
/// built-in template uses them (documented gap for exotic config commands).
pub fn expand_command(template: &str, arguments: &str) -> String {
    let args = split_command_args(arguments);
    // max placeholder position
    let mut last = 0usize;
    let b = template.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'$' && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if let Some(n) = std::str::from_utf8(&b[i + 1..j])
                .ok()
                .and_then(|d| d.parse::<usize>().ok())
            {
                last = last.max(n);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    // replace $N
    let mut with_args = String::new();
    let mut i = 0;
    let mut had_placeholder = false;
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let n: usize = std::str::from_utf8(&b[i + 1..j])
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            had_placeholder = true;
            let arg_index = n.saturating_sub(1);
            let value = if arg_index >= args.len() {
                String::new()
            } else if n == last {
                args[arg_index..].join(" ")
            } else {
                args[arg_index].clone()
            };
            with_args.push_str(&value);
            i = j;
        } else {
            // copy one utf-8 char
            let ch_len = {
                let ch = template[i..].chars().next().unwrap_or(' ');
                ch.len_utf8()
            };
            with_args.push_str(&template[i..i + ch_len]);
            i += ch_len;
        }
    }
    let uses_arguments = template.contains("$ARGUMENTS");
    let mut expanded = with_args.replace("$ARGUMENTS", arguments);
    if !had_placeholder && !uses_arguments && !arguments.trim().is_empty() {
        expanded = format!("{expanded}\n\n{arguments}");
    }
    expanded.trim().to_string()
}

/// v1 Session.Event.Error on unknown commands (oc-remote toast via
/// session.error SSE) + durable+s twin.
fn emit_session_error_event(st: &Arc<AppState>, sid: &str, message: &str) {
    let props = json!({
        "sessionID": sid,
        "error": {"name": "UnknownError", "data": {"message": message}},
    });
    if let Err(e) = ocserve_store::append_event(&st.writer, Some(sid), "session.error", &props) {
        tracing::warn!("session.error persist failed: {e:#}");
    }
    let seq = ocserve_store::next_event_seq(&st.db, sid).unwrap_or(0);
    let dir = st.paths["directory"].as_str().unwrap_or("/");
    st.bus.publish(ocserve_core::event::frame(
        dir,
        "session.error",
        props.clone(),
    ));
    st.bus.publish(ocserve_core::event::sync_frame(
        dir,
        "session.error",
        props,
        seq as u64,
        sid,
    ));
}

/// Busy rejection (v1 mapBusy → SessionBusyError): command/shell never queue
/// — they fail fast when the session is mid-prompt (upstream semantics).
fn session_busy(id: &str) -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        name: "SessionBusyError",
        message: format!("Session is busy: {id}"),
    }
}

/// POST /session/{id}/command — v1 promptSvc.command: look up the command
/// (built-ins init/review + config), expand the template, run the prompt
/// synchronously, return WithParts. Unknown → session.error SSE + 400.
async fn post_command(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let name = payload
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let arguments = payload
        .get("arguments")
        .and_then(|a| a.as_str())
        .unwrap_or("");
    // scoped: parking_lot guards are !Send — never held across run_prompt's
    // .await (Handler would stop accepting the future)
    let (template, available_owned): (Option<String>, Vec<String>) = {
        let payloads = st.payloads.read();
        let t = payloads
            .command
            .iter()
            .find(|c| c["name"].as_str() == Some(name))
            .and_then(|c| c["template"].as_str())
            .map(str::to_string);
        let names = payloads
            .command
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect();
        (t, names)
    };
    let available: Vec<&str> = available_owned.iter().map(String::as_str).collect();
    let Some(template) = template.as_deref() else {
        let msg = format!(
            "Command not found: \"{name}\".{}",
            if available.is_empty() {
                String::new()
            } else {
                format!(" Available commands: {}", available.join(", "))
            }
        );
        emit_session_error_event(&st, &id, &msg);
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: msg,
        });
    };
    let expanded = expand_command(template, arguments);
    let mut cmd_payload = json!({
        "parts": [{"type": "text", "text": expanded}],
    });
    if let Some(a) = payload.get("agent").and_then(|a| a.as_str()) {
        cmd_payload["agent"] = json!(a);
    }
    if let Some(m) = payload.get("model") {
        cmd_payload["model"] = m.clone();
    }
    if let Some(mid) = payload.get("messageID").and_then(|m| m.as_str()) {
        cmd_payload["messageId"] = json!(mid);
    }
    // v1 parity: command.execute.before (prompt.ts:1461) — input
    // {command, sessionID, arguments}, output {parts} mutated before the
    // prompt runs. Fail-open (no sidecar/hook error → parts unchanged).
    let parts_out = ocserve_core::prompt::hook_mutate(
        st.plugins.get(),
        "command.execute.before",
        json!({
            "command": name,
            "sessionID": id,
            "arguments": arguments,
        }),
        json!({"parts": cmd_payload["parts"].clone()}),
    )
    .await;
    if let Some(p) = parts_out.get("parts").and_then(|p| p.as_array()) {
        cmd_payload["parts"] = Value::Array(p.clone());
    }

    let ctx = build_prompt_context(&st, &cmd_payload, &id)?;
    let _release = lock_session(&st, &id).await?;
    let Ok(_guard) = _release.arc().try_lock() else {
        return Err(session_busy(&id));
    };
    let writer = st.writer.clone();
    let (info, parts) = ocserve_core::prompt::run_prompt_with(
        &ctx,
        &writer,
        &id,
        &cmd_payload,
        ocserve_core::prompt::RunOpts {
            // K-TITLE: a command must never name the session after itself
            auto_title: false,
            ..Default::default()
        },
    )
    .await
    .map_err(prompt_err)?;
    drop(_guard);
    Ok(Json(json!({"info": info, "parts": parts})))
}

/// POST /session/{id}/shell — v1 promptSvc.shell: NO model. Runs the command
/// directly (bash), records synthetic-user + assistant/bash tool messages,
/// returns WithParts. Port note: shell.env plugin hook skipped (no configured
/// plugin subscribes); abort-of-running-shell not wired (documented).
async fn post_shell(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let exists_id = id.clone();
    if !run_blocking(&st.db, move |db| {
        ocserve_store::session_exists(db, &exists_id)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        name: "InternalError",
        message: format!("{e:#}"),
    })? {
        return Err(ApiError::not_found(format!("Session not found: {id}")));
    }
    let command = payload
        .get("command")
        .and_then(|c| c.as_str())
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "command required".into(),
        })?;
    let agent = payload
        .get("agent")
        .and_then(|a| a.as_str())
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            name: "BadRequest",
            message: "agent required".into(),
        })?;
    let (pid, mid) = resolve_model(&st, &payload, &id);

    let _release = lock_session(&st, &id).await?;
    let Ok(_guard) = _release.arc().try_lock() else {
        return Err(session_busy(&id));
    };
    let dir = st.paths["directory"].as_str().unwrap_or("/").to_string();

    // busy → run → idle (client spinner parity)
    let status_frame = |kind: &str| {
        ocserve_core::event::frame(
            &dir,
            "session.status",
            json!({"sessionID": id, "status": {"type": kind}}),
        )
    };
    st.bus.publish(status_frame("busy"));

    let started = now_ms_local();
    let user_msg_id = ocserve_core::ids::msg_id();
    let user_part = json!({
        "type": "text", "id": ocserve_core::ids::prt_id(),
        "sessionID": id, "messageID": user_msg_id,
        "text": "The following tool was executed by the user",
        "synthetic": true,
    });
    let user_info = json!({
        "id": user_msg_id,
        "sessionID": id,
        "role": "user",
        "agent": agent,
        "model": {"providerID": pid, "modelID": mid},
        "time": {"created": started},
        "summary": {"diffs": []},
    });
    let assistant_id = ocserve_core::ids::msg_id();
    let call_id = ocserve_core::ids::evt_id();
    let mut tool_part = json!({
        "type": "tool", "id": ocserve_core::ids::prt_id(),
        "sessionID": id, "messageID": assistant_id,
        "callID": call_id, "tool": "bash",
        "state": {
            "status": "running",
            "input": {"command": command},
            "time": {"start": started},
        },
    });

    // persist user msg + assistant msg, emit running state (upstream order)
    let writer = st.writer.clone();
    insert_message_http(
        &writer,
        &st,
        &id,
        &user_info,
        std::slice::from_ref(&user_part),
    )?;
    let assistant_info = json!({
        "id": assistant_id,
        "parentID": user_msg_id,
        "role": "assistant",
        "mode": agent,
        "agent": agent,
        "path": {"cwd": dir, "root": "/"},
        "cost": 0.0,
        "tokens": {"total": 0, "input": 0, "output": 0, "reasoning": 0,
                   "cache": {"write": 0, "read": 0}},
        "modelID": mid,
        "providerID": pid,
        "time": {"created": started, "completed": started},
        "finish": "stop",
        "id": assistant_id,
        "sessionID": id,
    });
    insert_message_http(&writer, &st, &id, &assistant_info, &[])?;
    emit_durable_http(
        &st,
        &id,
        "message.updated",
        json!({"sessionID": id, "info": user_info.clone()}),
    )?;
    emit_durable_http(
        &st,
        &id,
        "message.updated",
        json!({"sessionID": id, "info": assistant_info.clone()}),
    )?;
    emit_durable_http(
        &st,
        &id,
        "message.part.updated",
        json!({"sessionID": id, "part": user_part}),
    )?;
    emit_durable_http(
        &st,
        &id,
        "message.part.updated",
        json!({"sessionID": id, "part": tool_part.clone()}),
    )?;

    // execute directly (bash executor: stderr merged, output bounded,
    // default 120s cap — ShellInput carries no timeout). Off-worker: a long
    // shell command must not pin a tokio worker (K-EFFICIENCY).
    let exec = {
        let cmd = json!({"command": command});
        let d = dir.clone();
        tokio::task::spawn_blocking(move || {
            ocserve_tools::execute("bash", &cmd, std::path::Path::new(&d))
        })
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("tool join: {e}"),
        })?
    };
    let (status, output) = match exec {
        Ok(r) => ("completed", r.output),
        Err(e) => ("error", format!("Error: {e:#}")),
    };
    let completed = now_ms_local();
    // v1 finish() shape: completed always carries {title:"", output,
    // metadata:{output}} — no exit field (capture-verified)
    tool_part["state"] = json!({
        "status": status,
        "input": {"command": command},
        "output": output,
        "title": "",
        "metadata": {"output": ""},
        "time": {"start": started, "end": completed},
    });
    tool_part["state"]["metadata"]["output"] = tool_part["state"]["output"].clone();
    insert_message_http(
        &writer,
        &st,
        &id,
        &assistant_info,
        std::slice::from_ref(&tool_part),
    )?;
    emit_durable_http(
        &st,
        &id,
        "message.part.updated",
        json!({"sessionID": id, "part": tool_part.clone()}),
    )?;
    st.bus.publish(status_frame("idle"));

    Ok(Json(json!({"info": assistant_info, "parts": [tool_part]})))
}

fn now_ms_local() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Persist a message via the store + emit its updated/part durable events.
fn insert_message_http(
    writer: &ocserve_store::Writer,
    st: &Arc<AppState>,
    sid: &str,
    info: &Value,
    parts: &[Value],
) -> Result<(), ApiError> {
    ocserve_store::insert_message(writer, Some(&*st.blobs), sid, info, parts).map_err(|e| {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        }
    })
}

/// Durable event from HTTP handlers (no PromptContext): persist + plain +
/// sync twin (seq from next_event_seq — single-flight per handler call).
fn emit_durable_http(
    st: &Arc<AppState>,
    sid: &str,
    event_type: &str,
    props: Value,
) -> Result<(), ApiError> {
    ocserve_store::append_event(&st.writer, Some(sid), event_type, &props).map_err(|e| {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        }
    })?;
    let seq = ocserve_store::next_event_seq(&st.db, sid).unwrap_or(0);
    let dir = st.paths["directory"].as_str().unwrap_or("/");
    st.bus
        .publish(ocserve_core::event::frame(dir, event_type, props.clone()));
    st.bus.publish(ocserve_core::event::sync_frame(
        dir, event_type, props, seq as u64, sid,
    ));
    Ok(())
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
    ocserve_metrics::gauge(
        "ocserve_sse_clients",
        st.bus.subscriber_count().max(1) as i64,
    );
    // The stream must own a bus clone: if the Router/state were dropped while
    // a subscriber still streams (oneshot tests), the Sender must survive.
    let bus_keepalive = st.bus.clone();
    // Drop-guard: decrement the client gauge when the stream ends (any exit).
    struct SseClientGuard;
    impl Drop for SseClientGuard {
        fn drop(&mut self) {
            ocserve_metrics::gauge_delta("ocserve_sse_clients", -1);
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
                            ocserve_metrics::counter("ocserve_sse_events_total", 1);
                            Some((Ok(Event::default().data(&frame)), (rx, next_hb, bus, guard)))
                        }
                        Ok(Err(_lagged_or_closed)) => {
                            ocserve_metrics::labeled_counter(
                                "ocserve_event_ring_lag_total",
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
    ocserve_metrics::counter("ocserve_requests_total", 1);
    let metric_t0 = std::time::Instant::now();
    let metric_label = format!(
        "route=\"{}\",method=\"{}\"",
        ocserve_metrics::route_label(req.uri().path()),
        req.method()
    );
    let record = move |t0: std::time::Instant| {
        ocserve_metrics::observe(
            "ocserve_http_request_duration_seconds",
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
        .route("/experimental/tool", get(get_experimental_tool))
        .route("/experimental/tool/ids", get(get_experimental_tool_ids))
        .route("/experimental/capabilities", get(get_capabilities))
        .route("/path", get(get_path))
        .route("/project", get(get_projects))
        .route("/project/current", get(get_project_current))
        .route("/session", get(get_sessions).post(post_session))
        .route("/session/{id}/fork", axum::routing::post(post_fork))
        .route("/session/status", get(session_status))
        .route("/session/search", axum::routing::post(search_messages))
        .route("/mcp/{name}/{action}", axum::routing::post(mcp_action))
        .route("/mcp/{name}/auth", axum::routing::delete(mcp_auth_remove))
        .route(
            "/auth/{providerID}",
            axum::routing::put(auth_put).delete(auth_delete),
        )
        .route("/provider/auth", axum::routing::get(provider_auth_methods))
        .route("/global/dispose", axum::routing::post(global_dispose))
        .route("/config", axum::routing::patch(patch_config))
        .route("/global/config", axum::routing::patch(patch_config))
        .route(
            "/session/{id}/summarize",
            axum::routing::post(post_summarize),
        )
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
        // upstream serves /event, /global/event and /api/event from ONE handler
        // (public.ts:155) — same handler here; oc-remote uses /global/event, TUI/SDK
        // probes hit /event (was a tolerated 404; now freeze-faithful)
        .route("/event", get(global_event))
        .route("/metrics", get(metrics))
        // TUI-attach probes (captured live; PLAN §2 hit-set expansion)
        .route("/api/location", get(get_api_location))
        .route("/api/agent", get(get_api_agent))
        .route("/api/command", get(get_api_command))
        .route("/api/reference", get(get_api_reference))
        .route("/experimental/resource", get(experimental_resource))
        .route("/experimental/workspace", get(experimental_workspace))
        // TUI attach calls this at boot (observed live 2026-10-08); freeze
        // answers [] with 200 under an empty workspace state.
        .route(
            "/experimental/workspace/status",
            get(experimental_workspace_status),
        )
        .route("/formatter", get(formatter_list))
        .route("/lsp", get(lsp_list))
        .route("/project/{id}/directories", get(project_directories))
        .route("/vcs", get(vcs_info))
        .route("/vcs/status", get(vcs_status))
        .route("/vcs/diff", get(vcs_diff))
        .route("/vcs/diff/raw", get(vcs_diff_raw))
        .route("/file/status", get(file_status))
        .route("/find/symbol", get(find_symbol))
        .route("/global/config", get(global_config_get))
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
            axum::routing::post(post_question_reply),
        )
        .route(
            "/question/{id}/reject",
            axum::routing::post(post_question_reject),
        )
        .route("/session/{id}/todo", get(get_todos))
        .route("/session/{id}/abort", axum::routing::post(post_abort))
        .route("/session/{id}/command", axum::routing::post(post_command))
        .route("/session/{id}/shell", axum::routing::post(post_shell))
        .route(
            "/session/{id}/message/{mid}",
            get(get_message_by_id).delete(delete_message_route),
        )
        // legacy permission respond (deprecated upstream; oc-remote uses
        // /permission/{id}/reply — this is the SDK/v1 shape)
        .route(
            "/session/{id}/permissions/{pid}",
            axum::routing::post(post_session_permission_respond),
        )
        // v1 control-plane log writer (web UI diagnostics)
        .route("/log", axum::routing::post(post_log))
        // revert/unrevert (K-REVERT): marker semantics only — the snapshot/
        // git-restore engine is out of MVP (divergence D-REVERT-NOSNAP,
        // PLAN §17). Marker shapes mirrored from session/revert.ts.
        .route("/session/{id}/revert", axum::routing::post(post_revert))
        .route("/session/{id}/unrevert", axum::routing::post(post_unrevert))
        // /instance/dispose — v1 single-instance dispose (freeze: true)
        .route("/instance/dispose", axum::routing::post(instance_dispose))
        // /session/{id}/init — built-in `init` command (guided AGENTS.md;
        // payload {messageID,providerID,modelID} — probed freeze)
        .route("/session/{id}/init", axum::routing::post(post_init))
        // provider OAuth: ocserve implements no OAuth flows; the route
        // exists and answers the freeze shape for non-OAuth providers
        // (400 BadRequest {}) instead of falling through to the SPA.
        .route(
            "/provider/{id}/oauth/authorize",
            axum::routing::post(post_provider_oauth),
        )
        .route(
            "/provider/{id}/oauth/callback",
            axum::routing::post(post_provider_oauth_callback),
        )
        // MCP OAuth: tagged shapes probed (unknown → 404
        // McpServerNotFoundError; known/non-OAuth → 400
        // McpUnsupportedOAuthError). No OAuth-capable servers in practice.
        .route(
            "/mcp/{name}/auth/authenticate",
            axum::routing::post(post_mcp_auth_authenticate),
        )
        .route(
            "/mcp/{name}/auth/callback",
            axum::routing::post(post_mcp_auth_callback),
        )
        .route(
            "/session/{id}/message/{mid}/part/{pid}",
            axum::routing::delete(delete_part_route).patch(patch_part_route),
        )
        .route("/experimental/session", get(get_experimental_sessions))
        // ---- /pty/* (web-UI terminal; freeze-probed 2026-10-08) ----
        .route("/pty/shells", get(pty::get_shells))
        .route("/pty", get(pty::list).post(pty::create))
        .route(
            "/pty/{id}",
            get(pty::get).put(pty::update).delete(pty::remove),
        )
        .route(
            "/pty/{id}/connect-token",
            axum::routing::post(pty::connect_token),
        )
        .route("/pty/{id}/connect", get(pty::connect))
        // /tui/* — external-controller ingress (PLAN §17; VS Code extension)
        .route(
            "/tui/append-prompt",
            axum::routing::post(tui::append_prompt),
        )
        .route("/tui/open-help", axum::routing::post(tui::open_help))
        .route(
            "/tui/open-sessions",
            axum::routing::post(tui::open_sessions),
        )
        .route("/tui/open-themes", axum::routing::post(tui::open_themes))
        .route("/tui/open-models", axum::routing::post(tui::open_models))
        .route(
            "/tui/submit-prompt",
            axum::routing::post(tui::submit_prompt),
        )
        .route("/tui/clear-prompt", axum::routing::post(tui::clear_prompt))
        .route(
            "/tui/execute-command",
            axum::routing::post(tui::execute_command),
        )
        .route("/tui/show-toast", axum::routing::post(tui::show_toast))
        .route("/tui/publish", axum::routing::post(tui::publish_event))
        .route(
            "/tui/select-session",
            axum::routing::post(tui::select_session),
        )
        .route("/tui/control/next", get(tui::control_next))
        .route(
            "/tui/control/response",
            axum::routing::post(tui::control_response),
        )
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
        // CORS (upstream cors.ts parity) sits OUTSIDE auth: preflights must
        // answer without credentials, exactly like upstream.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::ui::cors_gate,
        ))
        // Catch-all: unmatched paths serve the web UI (embedded upstream-side
        // or proxied to app.opencode.ai), matching v1 serveUIEffect. Disabled
        // via OCSERVE_UI=0 → JSON 404.
        .fallback(crate::ui::ui_fallback)
        // Upstream's catch-all is a *route*, not a fallback: a known path with
        // an unknown method (e.g. PATCH /pty/{id}) also gets the SPA, not a
        // 405. Probed against freeze 2026-10-08.
        .method_not_allowed_fallback(crate::ui::ui_fallback)
        .with_state(state)
}

/// CORS extras: OCSERVE_CORS (comma-separated) — the `--cors` equivalent.
/// Config-file `server.cors` support arrives with the config passthrough;
/// env is the runtime-tunable equivalent until then.
fn read_cors_env() -> Vec<String> {
    std::env::var("OCSERVE_CORS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
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

#[cfg(test)]
mod limit_matrix {
    use super::parse_limit;

    #[test]
    fn matches_probe_envelopes() {
        // Byte strings from message_page_contract.json probes (upstream 1.18.31)
        assert_eq!(
            parse_limit("abc").unwrap_err(),
            "Expected an integer, got NaN\n  at [\"limit\"]"
        );
        assert_eq!(
            parse_limit("5.5").unwrap_err(),
            "Expected an integer, got 5.5\n  at [\"limit\"]"
        );
        assert_eq!(
            parse_limit("-0.5").unwrap_err(),
            "Expected an integer, got -0.5\n  at [\"limit\"]"
        );
        assert_eq!(
            parse_limit("-1").unwrap_err(),
            "Expected a value greater than or equal to 0, got -1\n  at [\"limit\"]"
        );
        assert_eq!(
            parse_limit("999999999999999999999").unwrap_err(),
            "Expected an integer, got 1e+21\n  at [\"limit\"]"
        );
        assert_eq!(
            parse_limit("Infinity").unwrap_err(),
            "Expected an integer, got Infinity\n  at [\"limit\"]"
        );
    }

    #[test]
    fn accepts_integers_like_js_number() {
        assert_eq!(parse_limit("+5"), Ok(5));
        assert_eq!(parse_limit("1e3"), Ok(1000)); // JS Number("1e3") = 1000
        assert_eq!(parse_limit("0"), Ok(0));
        assert_eq!(parse_limit("9007199254740991"), Ok(9007199254740991)); // 2^53-1
    }
}

#[cfg(test)]
mod f1_blocking_tests {
    use super::{AppState, Payloads, router, run_blocking};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// PERF-10X F1: a blocking store call must NOT park the async worker.
    /// Single-worker runtime makes this discriminating: with the call inlined
    /// (the pre-F1 shape) the ticker cannot advance while the call runs
    /// (after == before → red); via the blocking pool it keeps ticking.
    /// M3: `GET /find/file`'s directory walk must execute OFF the tokio
    /// worker. Structural proof via thread identity, not timing: a ticker-
    /// based version of this test passed with the walk still inline (a few
    /// thousand files walk in microseconds), so it proved nothing.
    ///
    /// The probe drives the REAL route — calling `walk_files` directly would
    /// bypass the very `spawn_blocking` under test, which the first version
    /// of this test did (and it failed for exactly that reason).
    #[tokio::test]
    async fn find_files_walk_runs_off_the_worker() {
        use tower::ServiceExt as _;
        let dir = std::env::temp_dir().join(format!(
            "ocserve-m3-walk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..200 {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }

        let mut payloads = Payloads::default();
        payloads.config["directory"] = serde_json::Value::String(dir.to_string_lossy().to_string());
        let st = AppState::with_payloads(None, payloads);

        let handler_thread = std::thread::current().id();
        let req = axum::http::Request::builder()
            .uri("/find/file?query=f1&limit=10")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tokio::spawn(async move { router(st).oneshot(req).await })
            .await
            .expect("route task")
            .expect("infallible");
        assert_eq!(resp.status(), 200);

        let walked_on = super::WALK_THREAD.lock().ok().and_then(|g| *g);
        let walked_on = walked_on.expect("walk_files must record its thread");
        assert_ne!(
            walked_on, handler_thread,
            "the walk ran on the async worker — blocking syscalls must go \
             through spawn_blocking (M3)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PERF-10X F1: a blocking store call must NOT park the async worker.
    /// Single-worker runtime makes this discriminating: with the call inlined
    /// (the pre-F1 shape) the ticker cannot advance while the call runs
    /// (after == before → red); via the blocking pool it keeps ticking.
    /// Negative control: swap run_blocking for the inline sleep → red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]

    async fn run_blocking_frees_the_worker() {
        let ticks = Arc::new(AtomicU64::new(0));
        let t = ticks.clone();
        let ticker = tokio::spawn(async move {
            let mut i = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                i += 1;
                t.store(i, Ordering::Relaxed);
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let before = ticks.load(Ordering::Relaxed);
        assert!(before > 0, "ticker must be running before the probe");

        // The probe runs as a SPAWNED task: only spawned tasks execute on
        // worker threads (the #[tokio::test] body itself runs on block_on's
        // thread — an inline sleep there parked nothing and the first version
        // of this test passed its own negative control; caught 2026-10-06).
        let probe = tokio::spawn(async move {
            run_blocking(std::path::Path::new("/nonexistent/f1-probe"), |_db| {
                std::thread::sleep(Duration::from_millis(250));
                Ok::<(), anyhow::Error>(())
            })
            .await
            .expect("probe op");
        });
        probe.await.expect("probe join");

        let after = ticks.load(Ordering::Relaxed);
        ticker.abort();
        assert!(
            after > before,
            "blocking call parked the worker: before={before} after={after}"
        );
    }
}

#[cfg(test)]
mod f5_wire {
    use super::*;

    /// Identity control: cached bytes == what axum's Json would produce
    /// (both serde_json::to_vec) — one value per key, no drift.
    #[test]
    fn wire_bytes_are_serde_identical() {
        let st = AppState::new();
        let p = st.payloads.read();
        let w = st.wire.read();
        for (k, v) in [
            ("config", &p.config),
            ("config_providers", &p.config_providers),
            ("provider", &p.provider),
            ("console", &p.console),
            ("capabilities", &p.capabilities),
        ] {
            let cached = w.get(k).unwrap_or_else(|| panic!("missing wire key {k}"));
            let want = serde_json::to_vec(v).unwrap();
            assert_eq!(
                &cached.identity[..],
                &want[..],
                "wire bytes for {k} must equal Value serialization"
            );
        }
        for (k, v) in [("agent", &p.agent), ("command", &p.command)] {
            let cached = w.get(k).unwrap_or_else(|| panic!("missing wire key {k}"));
            let want = serde_json::to_vec(v).unwrap();
            assert_eq!(&cached.identity[..], &want[..]);
        }
    }

    /// Kill-switch: wire_off => no cache, wire_json falls back (None).
    #[test]
    fn wire_off_disables_cache() {
        let empty = rebuild_wire(&Payloads::default(), true);
        assert!(empty.is_empty(), "off => nothing serialized");
        let st = AppState::new();
        st.wire_off
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(wire_json(&st, "config").is_none(), "off => Value path");
        st.wire_off
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(wire_json(&st, "config").is_some(), "on => Bytes path");
    }
}
