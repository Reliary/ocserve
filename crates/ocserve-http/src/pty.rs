//! `/pty/*` — terminal session routes (web-UI integrated terminal, VS Code,
//! SDK clients). Freeze parity: shapes captured live against opencode
//! 1.18.31 (2026-10-08, bench/pty/PTY-PLAN.md).
//!
//! Routes:
//!   GET    /pty/shells                → Shell[] (no process spawned)
//!   GET    /pty                       → Info[] (running only — legacy surface)
//!   POST   /pty                       → Info (spawns; login shells get -l)
//!   GET    /pty/{id}                  → Info | 404 tag | 400 Params envelope
//!   PUT    /pty/{id}                  → Info (title/size; PATCH not a route)
//!   DELETE /pty/{id}                  → true
//!   POST   /pty/{id}/connect-token    → {ticket,expires_in} (header + origin gated)
//!   GET    /pty/{id}/connect          → websocket: replay → meta([0x00]JSON)
//!                                        → live; close 4404 on not-found/exited

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::{ApiError, AppState, HttpError};

/// v1 pty ids: `pty_` + 26 chars (schema/src/pty.ts IDSchema isStartsWith).
/// Malformed path params get the Effect Query Params envelope (probed).
fn check_pty_id(id: &str) -> Result<(), ApiError> {
    let ok = id.starts_with("pty_") && id.len() > 4;
    if ok {
        return Ok(());
    }
    Err(ApiError {
        status: StatusCode::BAD_REQUEST,
        name: "BadRequest",
        message: format!("Expected a string starting with \"pty\", got \"{id}\"\n  at [\"ptyID\"]"),
    })
}

fn not_found(id: &str) -> HttpError {
    // Tagged envelope (probed): {"_tag":"PtyNotFoundError","ptyID","message"}
    HttpError::TaggedData {
        status: StatusCode::NOT_FOUND,
        tag: "PtyNotFoundError",
        fields: json!({"ptyID": id, "message": format!("PTY session not found: {id}")}),
    }
}

pub async fn get_shells(State(st): State<Arc<AppState>>) -> Response {
    axum::Json(st.pty.shells()).into_response()
}

pub async fn list(State(st): State<Arc<AppState>>) -> Response {
    axum::Json(Value::Array(st.pty.list_running())).into_response()
}

pub async fn create(State(st): State<Arc<AppState>>, body: Option<axum::Json<Value>>) -> Response {
    let input = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    match st.pty.create(&input) {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"name": "UnknownError", "data": {"message": e}})),
        )
            .into_response(),
    }
}

pub async fn get(Path(id): Path<String>, State(st): State<Arc<AppState>>) -> Response {
    if let Err(e) = check_pty_id(&id) {
        return e.into_response();
    }
    match st.pty.get_running(&id) {
        Ok(info) => axum::Json(info).into_response(),
        Err(_) => not_found(&id).into_response(),
    }
}

pub async fn update(
    Path(id): Path<String>,
    State(st): State<Arc<AppState>>,
    body: Option<axum::Json<Value>>,
) -> Response {
    if let Err(e) = check_pty_id(&id) {
        return e.into_response();
    }
    let input = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    match st.pty.update(&id, &input) {
        Ok(info) => axum::Json(info).into_response(),
        Err(_) => not_found(&id).into_response(),
    }
}

pub async fn remove(Path(id): Path<String>, State(st): State<Arc<AppState>>) -> Response {
    if let Err(e) = check_pty_id(&id) {
        return e.into_response();
    }
    match st.pty.remove(&id) {
        Ok(v) => axum::Json(v).into_response(),
        Err(_) => not_found(&id).into_response(),
    }
}

/// POST /pty/{id}/connect-token — upstream requires the marker header
/// (`x-opencode-ticket: 1`) AND a permitted Origin, then the session must
/// exist. Wrong header/origin → 403 tag; unknown id (after the gate) → 404.
pub async fn connect_token(
    Path(id): Path<String>,
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check_pty_id(&id) {
        return e.into_response();
    }
    let marker_ok = headers
        .get("x-opencode-ticket")
        .and_then(|v| v.to_str().ok())
        == Some("1");
    let origin_ok = match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        None => true, // no Origin: allowed (CLI clients)
        Some(o) => {
            crate::ui::is_allowed_cors_origin(o, &st.cors_extra)
                || headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(|h| o.ends_with(h))
                    .unwrap_or(false)
        }
    };
    if !marker_ok || !origin_ok {
        return (HttpError::TaggedData {
            status: StatusCode::FORBIDDEN,
            tag: "PtyForbiddenError",
            fields: json!({"message": "Invalid PTY connect token request"}),
        })
        .into_response();
    }
    if st.pty.get_running(&id).is_err() {
        return not_found(&id).into_response();
    }
    axum::Json(st.pty.issue_ticket(&id)).into_response()
}

#[derive(serde::Deserialize)]
pub struct ConnectQuery {
    pub cursor: Option<String>,
    pub ticket: Option<String>,
    #[allow(dead_code)]
    pub directory: Option<String>,
    #[allow(dead_code)]
    pub workspace: Option<String>,
}

/// GET /pty/{id}/connect — websocket. Upstream: if a ticket is presented it
/// must consume; if not, allowed origins only. Then attach, replay
/// (64 KiB chunks), meta frame [0x00 + {"cursor":N}], live streaming;
/// close code 4404 when the session is not-found/exited.
pub async fn connect(
    Path(id): Path<String>,
    Query(q): Query<ConnectQuery>,
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(e) = check_pty_id(&id) {
        return e.into_response();
    }
    if let Some(ticket) = q.ticket.as_deref() {
        let origin_ok = match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            None => true,
            Some(o) => crate::ui::is_allowed_cors_origin(o, &st.cors_extra),
        };
        if !origin_ok || !st.pty.consume_ticket(&id, ticket) {
            return StatusCode::FORBIDDEN.into_response();
        }
    } else {
        // no ticket: permitted origin required (probed: localhost origin OK)
        if let Some(o) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
            && !crate::ui::is_allowed_cors_origin(o, &st.cors_extra)
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let cursor = q.cursor.and_then(|c| c.parse::<i64>().ok());
    match st.pty.attach(&id, cursor) {
        Err(_) => {
            // upgrade then immediately close 4404 (upstream behavior)
            ws.on_upgrade(move |mut socket| async move {
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4404,
                        reason: "session not found".into(),
                    })))
                    .await;
            })
        }
        Ok(attachment) => {
            let st2 = st.clone();
            let id2 = id.clone();
            ws.on_upgrade(move |socket| pty_ws_loop(socket, st2, id2, attachment))
        }
    }
}

async fn pty_ws_loop(
    mut socket: WebSocket,
    st: Arc<AppState>,
    id: String,
    mut attachment: ocserve_pty::Attachment,
) {
    // 1) replay in 64 KiB chunks (upstream PtyProtocol.REPLAY_CHUNK)
    const REPLAY_CHUNK: usize = 64 * 1024;
    let replay = attachment.replay.clone();
    let mut rest = replay.as_str();
    while !rest.is_empty() {
        let cut = floor_char_boundary(rest, REPLAY_CHUNK);
        if socket
            .send(Message::Text(rest[..cut].to_string().into()))
            .await
            .is_err()
        {
            st.pty.detach(&id, attachment.sub_id);
            return;
        }
        rest = &rest[cut..];
    }
    // 2) meta frame: 0x00 then JSON {"cursor":N}
    let meta = format!("\u{0}{}", json!({"cursor": attachment.cursor}));
    if socket.send(Message::Text(meta.into())).await.is_err() {
        st.pty.detach(&id, attachment.sub_id);
        return;
    }
    // 3) live: pump subscription → socket; inbound → pty write; close on end.
    loop {
        tokio::select! {
            msg = attachment.rx.recv() => match msg {
                Some(ocserve_pty::Msg::Data(chunk)) => {
                    if socket.send(Message::Text(chunk.into())).await.is_err() { break; }
                }
                Some(ocserve_pty::Msg::End { .. }) | None => {
                    let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1000, reason: "".into(),
                    }))).await;
                    break;
                }
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(t))) => attachment_write(&st, &id, t.as_str()),
                Some(Ok(Message::Binary(b))) => {
                    attachment_write(&st, &id, &String::from_utf8_lossy(&b));
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
    st.pty.detach(&id, attachment.sub_id);
}

fn attachment_write(st: &Arc<AppState>, id: &str, data: &str) {
    st.pty.write_session(id, data);
}

/// Largest char boundary ≤ `want` (may be 0 when a single char exceeds).
fn floor_char_boundary(s: &str, want: usize) -> usize {
    if want >= s.len() {
        return s.len();
    }
    let mut end = want;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 { want.min(s.len()) } else { end }
}
