//! refine-http: axum router, auth, byte-golden SSE, upstream error envelope.

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Freeze (PLAN §3): version reported by `/global/health`.
pub const FREEZE_VERSION: &str = "1.18.31";

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
    /// Agents payload for GET /agent.
    pub agent: Vec<serde_json::Value>,
    /// Known sessions (M1: served from store as it comes online).
    pub sessions: parking_lot::RwLock<HashMap<String, serde_json::Value>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: json!({}), // replaced by CLI with recorded corpus
            agent: vec![],
            sessions: parking_lot::RwLock::new(HashMap::new()),
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

async fn get_sessions(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let mut v: Vec<serde_json::Value> = st.sessions.read().values().cloned().collect();
    v.sort_by(|a, b| {
        b["time"]["updated"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["time"]["updated"].as_i64().unwrap_or(0))
    });
    Json(v)
}

async fn get_session(
    State(st): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    st.sessions
        .read()
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
}

async fn session_status() -> impl IntoResponse {
    Json(json!({}))
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
async fn global_event() -> Response {
    let connected = Event::default().data(
        json!({"payload":{"id":evt_id("connected"),"type":"server.connected","properties":{}}})
            .to_string(),
    );
    let events = stream::once(async move { Ok::<_, std::convert::Infallible>(connected) })
        .chain(stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let ev = Event::default().data(
                json!({"payload":{"id":evt_id("heartbeat"),"type":"server.heartbeat","properties":{}}})
                    .to_string(),
            );
            Some((Ok(ev), ()))
        }));
    // KeepAlive frames disabled: upstream sends no retry/comment keep-alive bytes.
    let mut resp = Sse::new(events)
        .keep_alive(KeepAlive::new().text("disabled-until-unreachable"))
        .into_response();
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

/// Router for the P0 surface. Auth (off per freeze) is layered by the CLI.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/global/health", get(health))
        .route("/config", get(get_config))
        .route("/agent", get(get_agent))
        .route("/session", get(get_sessions))
        .route("/session/status", get(session_status))
        .route("/session/{id}", get(get_session))
        .route("/global/event", get(global_event))
        .with_state(state)
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
