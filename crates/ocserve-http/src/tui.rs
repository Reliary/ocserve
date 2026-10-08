//! `/tui/*` external-controller group (PLAN §17; freeze parity probed live
//! 2026-10-08 against upstream 1.18.31).
//!
//! Semantics (v1 groups/tui.ts + handlers/tui.ts): every endpoint publishes
//! onto the global event stream and returns `true`; the attached TUI
//! (packages/tui/src/app.tsx:987) consumes `tui.command.execute`,
//! `tui.toast.show`, `tui.session.select`, `tui.prompt.append` and acts.
//! Primary external caller: the VS Code extension's `/tui/append-prompt`.
//!
//! Faithful quirks kept: `open-themes` publishes `session.list` (upstream
//! bug, replicated); unknown `execute-command` values publish an event whose
//! `properties` omit `command` (JS `undefined` drops from JSON); toast
//! `duration` defaults to 5000.
//!
//! Control rendezvous (`/tui/control/next` GET long-poll + `/response` POST)
//! is the queue pair from v1 shared/tui-control.ts. No production caller
//! exists in-tree (test harness only); implemented with bounded channels so
//! a client disconnect releases the waiter.

use crate::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;

/// Bounded rendezvous for the control queue (AGENTS §2.3: every channel has a
/// declared bound). Depth 16 is far beyond any observed use; submitters block
/// only when the queue is full and no consumer is polling.
const CONTROL_CAP: usize = 16;

pub struct TuiControl {
    tx_req: tokio::sync::mpsc::Sender<Value>,
    rx_req: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Value>>,
    /// Response side (no production consumer today — in-process only).
    #[allow(dead_code)]
    tx_res: tokio::sync::mpsc::Sender<Value>,
    #[allow(dead_code)]
    rx_res: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Value>>,
}

impl Default for TuiControl {
    fn default() -> Self {
        let (tx_req, rx_req) = tokio::sync::mpsc::channel(CONTROL_CAP);
        let (tx_res, rx_res) = tokio::sync::mpsc::channel(CONTROL_CAP);
        Self {
            tx_req,
            rx_req: tokio::sync::Mutex::new(rx_req),
            tx_res,
            rx_res: tokio::sync::Mutex::new(rx_res),
        }
    }
}

impl TuiControl {
    /// Long-poll: resolves with the next queued request, or `None` when the
    /// client disconnects (the handler future is dropped by axum).
    pub async fn next_request(&self) -> Option<Value> {
        self.rx_req.lock().await.recv().await
    }

    /// `false` = no consumer holds the response receiver (queue gone).
    pub async fn submit_response(&self, body: Value) -> bool {
        self.tx_res.send(body).await.is_ok()
    }

    /// Producer side of the request queue (used by in-process consumers /
    /// tests; no production caller today — mirrors upstream).
    #[allow(dead_code)]
    pub async fn submit_request(&self, body: Value) -> bool {
        self.tx_req.send(body).await.is_ok()
    }
}

/// v1 commandAliases: legacy names → canonical keymap commands.
const COMMAND_ALIASES: &[(&str, &str)] = &[
    ("session_new", "session.new"),
    ("session_share", "session.share"),
    ("session_interrupt", "session.interrupt"),
    ("session_compact", "session.compact"),
    ("messages_page_up", "session.page.up"),
    ("messages_page_down", "session.page.down"),
    ("messages_line_up", "session.line.up"),
    ("messages_line_down", "session.line.down"),
    ("messages_half_page_up", "session.half.page.up"),
    ("messages_half_page_down", "session.half.page.down"),
    ("messages_first", "session.first"),
    ("messages_last", "session.last"),
    ("agent_cycle", "agent.cycle"),
];

fn publish(st: &Arc<AppState>, event_type: &str, props: Value) {
    let dir = st.paths["directory"].as_str().unwrap_or("/");
    st.bus
        .publish(ocserve_core::event::frame(dir, event_type, props));
}

fn publish_command(st: &Arc<AppState>, command: Option<&str>) {
    // JS `{ command: undefined }` serializes to `{}` — replicate exactly.
    let props = match command {
        Some(c) => json!({"command": c}),
        None => json!({}),
    };
    publish(st, "tui.command.execute", props);
}

/// Effect payload-decode envelope (probed live: `/tui/select-session` with a
/// non-`ses` id returns this shape).
pub fn bad_request_payload(message: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "name": "BadRequest",
            "data": { "message": message, "kind": "Payload" }
        })),
    )
        .into_response()
}

fn not_found(message: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "name": "NotFoundError",
            "data": { "message": message }
        })),
    )
        .into_response()
}

// Individual endpoints (each publishes one event and answers `true`).

pub async fn append_prompt(
    State(st): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    let text = body
        .as_ref()
        .and_then(|Json(v)| v.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    publish(&st, "tui.prompt.append", json!({"text": text}));
    Json(json!(true))
}

pub async fn open_help(State(st): State<Arc<AppState>>) -> Json<Value> {
    publish_command(&st, Some("help.show"));
    Json(json!(true))
}

pub async fn open_sessions(State(st): State<Arc<AppState>>) -> Json<Value> {
    publish_command(&st, Some("session.list"));
    Json(json!(true))
}

/// Upstream quirk, replicated deliberately (handlers/tui.ts:51-54):
/// open-themes publishes `session.list`.
pub async fn open_themes(State(st): State<Arc<AppState>>) -> Json<Value> {
    publish_command(&st, Some("session.list"));
    Json(json!(true))
}

pub async fn open_models(State(st): State<Arc<AppState>>) -> Json<Value> {
    publish_command(&st, Some("model.list"));
    Json(json!(true))
}

pub async fn submit_prompt(State(st): State<Arc<AppState>>) -> Json<Value> {
    publish_command(&st, Some("prompt.submit"));
    Json(json!(true))
}

pub async fn clear_prompt(State(st): State<Arc<AppState>>) -> Json<Value> {
    publish_command(&st, Some("prompt.clear"));
    Json(json!(true))
}

pub async fn execute_command(
    State(st): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(Json(v)) = body else {
        return bad_request_payload("Expected object".into());
    };
    let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
        return bad_request_payload("Expected a string at [\"command\"]".into());
    };
    let canonical = COMMAND_ALIASES
        .iter()
        .find(|(legacy, _)| *legacy == cmd)
        .map(|(_, canon)| *canon);
    publish_command(&st, canonical);
    Json(json!(true)).into_response()
}

pub async fn show_toast(State(st): State<Arc<AppState>>, body: Option<Json<Value>>) -> Json<Value> {
    let v = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let mut props = serde_json::Map::new();
    if let Some(t) = v.get("title").and_then(|t| t.as_str()) {
        props.insert("title".into(), json!(t));
    }
    props.insert(
        "message".into(),
        json!(
            v.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
        ),
    );
    props.insert(
        "variant".into(),
        json!(v.get("variant").and_then(|x| x.as_str()).unwrap_or("info")),
    );
    props.insert(
        "duration".into(),
        json!(v.get("duration").and_then(|d| d.as_u64()).unwrap_or(5000)),
    );
    publish(&st, "tui.toast.show", Value::Object(props));
    Json(json!(true))
}

/// Union dispatch (v1 handlers/tui.ts:86-96): unknown types answer `true`
/// without publishing.
pub async fn publish_event(
    State(st): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    if let Some(Json(v)) = body {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        let props = v.get("properties").cloned().unwrap_or_else(|| json!({}));
        match ty {
            "tui.prompt.append" => publish(&st, "tui.prompt.append", props),
            "tui.command.execute" => publish(&st, "tui.command.execute", props),
            "tui.toast.show" => publish(&st, "tui.toast.show", props),
            "tui.session.select" => publish(&st, "tui.session.select", props),
            _ => {}
        }
    }
    Json(json!(true))
}

pub async fn select_session(
    State(st): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(Json(v)) = body else {
        return bad_request_payload("Expected object".into());
    };
    let Some(sid) = v.get("sessionID").and_then(|s| s.as_str()) else {
        return bad_request_payload("Expected a string at [\"sessionID\"]".into());
    };
    if !sid.starts_with("ses") {
        // Exact upstream message (probed live, including the location trailer).
        return bad_request_payload(format!(
            "Expected a string starting with \"ses\", got \"{sid}\"\n  at [\"sessionID\"]"
        ));
    }
    let exists = {
        let sid = sid.to_string();
        crate::run_blocking(&st.db, move |db| ocserve_store::session_exists(db, &sid))
            .await
            .unwrap_or(false)
    };
    if !exists {
        return not_found(format!("Session not found: {sid}"));
    }
    publish(&st, "tui.session.select", json!({"sessionID": sid}));
    Json(json!(true)).into_response()
}

pub async fn control_next(State(st): State<Arc<AppState>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match st.tui.next_request().await {
        Some(req) => Json(req).into_response(),
        // Client went away while waiting; axum will discard the response.
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

pub async fn control_response(
    State(st): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    let v = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let _ = st.tui.submit_response(v).await;
    Json(json!(true))
}
