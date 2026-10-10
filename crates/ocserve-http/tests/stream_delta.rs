//! K-STREAMDELTA — the 2026-10-11 "web UI shows nothing until reload" class.
//!
//! Root cause: ocserve streamed assistant text via `message.part.delta` with
//! `partID: ""` (and `messageID` = the USER message), so the web UI's reducer —
//! which keys deltas by `partID` against an already-seen part
//! (`store.part[messageID]` → `search(r.partID)`) — silently dropped every
//! delta. The response only appeared after a reload, because the full text
//! part is persisted at turn end and fetched fresh. Upstream mints the
//! assistant MessageID + text PartID BEFORE the turn and emits a
//! `message.part.updated` start part before any `updatePartDelta`
//! (prompt.ts:1186-1201, processor.ts:280-291/500-511).
//!
//! These tests drive a REAL provider turn against a stub SSE provider (no
//! LLM), capture the bus frames, and assert the streaming contract end to end:
//!   1. an assistant `message.updated` with NO `time.completed` (streaming)
//!      and a `msg_` id is published before any delta,
//!   2. a `message.part.updated` text part with a `prt_` id precedes its deltas,
//!   3. every `message.part.delta` carries a non-empty `^prt` `partID` and the
//!      assistant (not user) `messageID`,
//!   4. the persisted final text part reuses the SAME id the deltas streamed to.
//!
//! `delta_violations` is the shared checker; the negative-control test feeds it
//! a planted pre-fix frame (`partID:""`, user messageID) and requires a flag —
//! so the checker can never pass vacuously (AGENTS §1).
use axum::body::Body;
use axum::http::Request;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::io::Write as _;
use std::sync::Arc;
use tower::ServiceExt;

/// Provider SSE with a reasoning delta then two text deltas + a stop + usage.
fn reasoning_text_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"PO\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"NG\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\
     \"completion_tokens\":5,\"total_tokens\":105}}\n\n\
     data: [DONE]\n\n"
}

fn spawn_provider(body: &'static str) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => return,
                    }
                    if let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&got[..head_end]).to_string();
                let clen = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let _ = sock.write_all(body.as_bytes());
                let _ = sock.flush();
            });
        }
    });
    addr
}

fn setup(addr: std::net::SocketAddr, sid: &str) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-sd-{}-{sid}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let mut limits = std::collections::HashMap::new();
    limits.insert(
        ("fake".to_string(), "m".to_string()),
        json!({"context": 1_000_000}),
    );
    let llm = LlmRegistry {
        limits,
        endpoints: [("fake".into(), (format!("http://{addr}"), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    let st = AppState::with_wiring(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    );
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": sid, "projectID": "global", "directory": "/w",
            "path": sid, "slug": sid, "title": "t", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

/// The shared delta-contract checker. Returns human-readable violations.
/// A planted pre-fix frame (`partID:""`, user messageID, no preceding start
/// part) MUST produce a violation — asserted by the negative-control test.
fn delta_violations(frames: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    // assistant ids announced via message.updated
    let mut assistant_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    // every part id announced via message.part.updated, per messageID
    let mut announced_parts: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut saw_live_skeleton = false; // assistant message.updated BEFORE any delta, no completed
    let mut saw_text_start = false;
    let mut seen_delta = false;

    for f in frames {
        let p = match f.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if p.get("type").and_then(Value::as_str) == Some("sync") {
            continue;
        }
        let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
        let props = p.get("properties").cloned().unwrap_or(Value::Null);
        match ty {
            "message.updated" => {
                let info = props.get("info").cloned().unwrap_or(Value::Null);
                if info.get("role").and_then(Value::as_str) == Some("assistant") {
                    let id = info
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !id.starts_with("msg_") {
                        out.push(format!(
                            "assistant message.updated id must be ^msg_, got {id:?}"
                        ));
                    }
                    // a live skeleton: published before any delta, no time.completed
                    if !seen_delta && info.pointer("/time/completed").is_none() {
                        saw_live_skeleton = true;
                    }
                    assistant_ids.insert(id);
                }
            }
            "message.part.updated" => {
                let part = props.get("part").cloned().unwrap_or(Value::Null);
                let mid = part
                    .get("messageID")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let pid = part
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if part.get("type").and_then(Value::as_str) == Some("text")
                    && part.get("text").and_then(Value::as_str) == Some("")
                {
                    saw_text_start = true;
                }
                announced_parts.entry(mid).or_default().insert(pid);
            }
            "message.part.delta" => {
                seen_delta = true;
                let mid = props.get("messageID").and_then(Value::as_str).unwrap_or("");
                let pid = props.get("partID").and_then(Value::as_str).unwrap_or("");
                if pid.is_empty() || !pid.starts_with("prt") {
                    out.push(format!(
                        "delta partID must match ^prt (got {pid:?}) — the web UI drops it"
                    ));
                }
                if !assistant_ids.contains(mid) {
                    out.push(format!(
                        "delta messageID {mid:?} is not an announced assistant message \
                         (upstream targets the assistant message, not the user)"
                    ));
                }
                let announced = announced_parts
                    .get(mid)
                    .map(|s| s.contains(pid))
                    .unwrap_or(false);
                if !announced {
                    out.push(format!(
                        "delta (messageID={mid:?}, partID={pid:?}) has no preceding \
                         message.part.updated start part — the reducer finds no part to accumulate into"
                    ));
                }
            }
            _ => {}
        }
    }
    if !saw_live_skeleton {
        out.push(
            "no live assistant message.updated (no time.completed) was emitted before the first delta".into(),
        );
    }
    if !saw_text_start {
        out.push("no empty-text message.part.updated start part was emitted".into());
    }
    out
}

async fn drain_bus(rx: &mut ocserve_core::event::EventBusReceiver) -> Vec<Value> {
    // The bus stores each frame as the encoded JSON object (the `data: ` SSE
    // prefix is added later by the stream handler). Delivery is synchronous on
    // publish, so every frame is buffered by the time the request returns.
    let mut out = Vec::new();
    while let Ok(s) = rx.try_recv() {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            out.push(v);
        }
    }
    out
}

async fn post_message(app: &axum::Router, sid: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/session/{sid}/message"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"parts": [{"type": "text", "text": "hi"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn streaming_deltas_carry_partid_and_precede_persist() {
    let addr = spawn_provider(reasoning_text_sse());
    let st = setup(addr, "ses_sd_ok");
    let app = ocserve_http::router(st.clone());
    let mut rx = st.bus.subscribe();

    let resp = post_message(&app, "ses_sd_ok").await;
    assert!(resp.status().is_success(), "turn failed");

    let frames = drain_bus(&mut rx).await;
    let viol = delta_violations(&frames);
    assert!(
        viol.is_empty(),
        "streaming-delta contract violations:\n  {}",
        viol.join("\n  ")
    );

    // 4. the persisted final text part reuses the streamed id: the last text
    // part.updated must share a messageID with the announced assistant ids.
    let mut streamed_text_ids: Vec<String> = Vec::new();
    let mut final_text_ids: Vec<String> = Vec::new();
    for f in &frames {
        let p = f.get("payload").unwrap();
        if p.get("type").and_then(Value::as_str) == Some("message.part.updated")
            && p.pointer("/properties/part/type").and_then(Value::as_str) == Some("text")
        {
            let pid = p
                .pointer("/properties/part/id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if p.pointer("/properties/part/time/end").is_some() {
                final_text_ids.push(pid);
            } else {
                streamed_text_ids.push(pid);
            }
        }
    }
    assert!(
        !final_text_ids.is_empty(),
        "no persisted (times.end) text part observed"
    );
    // every delta-targeted text id must be one the final part reuses
    for d in &frames {
        let p = d.get("payload").unwrap();
        if p.get("type").and_then(Value::as_str) == Some("message.part.delta")
            && p.pointer("/properties/field").and_then(Value::as_str) == Some("text")
        {
            let pid = p
                .pointer("/properties/partID")
                .and_then(Value::as_str)
                .unwrap_or("");
            assert!(
                final_text_ids.iter().any(|f| f == pid),
                "streamed text partID {pid:?} not reused by the persisted part {final_text_ids:?}"
            );
        }
    }
}

/// Negative control (AGENTS §1): the checker MUST flag the exact pre-fix shape.
/// If it does not, the guard is vacuous.
#[test]
fn delta_checker_flags_the_pre_fix_shape() {
    let pre_fix = vec![
        json!({"payload": {"type": "message.updated", "properties": {"info": {
            "id": "msg_assistant01", "role": "assistant",
            "time": {"created": 1}, "sessionID": "ses_x"
        }}}}),
        // pre-fix delta: empty partID, user messageID, no start part
        json!({"payload": {"type": "message.part.delta", "properties": {
            "sessionID": "ses_x", "messageID": "msg_user01",
            "partID": "", "field": "text", "delta": "P"
        }}}),
    ];
    let viol = delta_violations(&pre_fix);
    assert!(
        viol.iter().any(|v| v.contains("partID must match")),
        "checker failed to flag empty partID: {viol:?}"
    );
    assert!(
        viol.iter()
            .any(|v| v.contains("not an announced assistant")),
        "checker failed to flag user-messageID targeting: {viol:?}"
    );
    // and the healthy shape must be clean (non-vacuous both directions)
    let healthy = vec![
        json!({"payload": {"type": "message.updated", "properties": {"info": {
            "id": "msg_assistant01", "role": "assistant",
            "time": {"created": 1}, "sessionID": "ses_x"
        }}}}),
        json!({"payload": {"type": "message.part.updated", "properties": {"part": {
            "type": "text", "id": "prt_text01", "messageID": "msg_assistant01",
            "sessionID": "ses_x", "text": ""
        }}}}),
        json!({"payload": {"type": "message.part.delta", "properties": {
            "sessionID": "ses_x", "messageID": "msg_assistant01",
            "partID": "prt_text01", "field": "text", "delta": "P"
        }}}),
    ];
    assert!(
        delta_violations(&healthy).is_empty(),
        "healthy frames must be clean: {:?}",
        delta_violations(&healthy)
    );
}
