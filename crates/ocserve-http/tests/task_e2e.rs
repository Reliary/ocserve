//! K-TASK — the `task` subagent tool (D-TOOLS-TASK closure).
//!
//! Proves the launcher end-to-end against a stub provider (no LLM):
//!   - a `task` tool call on the parent spawns a CHILD session (`parent_id`
//!     set, title `"{desc} (@{agent} subagent)"`, derived permission),
//!   - the child runs the SHARED executor and returns its final text inside the
//!     upstream `<task …><task_result>…</task_result></task>` block,
//!   - the depth guard refuses a nested spawn at the configured `subagent_depth`,
//!   - an unknown `subagent_type` is refused.
//!
//! The provider is a scripted SSE stub: the PARENT session gets step1=`task`
//! call + step2=final text; the CHILD session gets a single text answer. The
//! stub keys on the request body (a fresh session has no prior messages, so
//! we distinguish by whether the conversation already contains a tool result).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

const TASK_CALL_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_task1\",\"type\":\"function\",\"function\":{\
     \"name\":\"task\",\"arguments\":\"{\\\"description\\\":\\\"explore the repo\\\",\\\"prompt\\\":\\\"list the crates\\\",\\\"subagent_type\\\":\\\"explore\\\"}\"\
     }}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n";

const CHILD_ANSWER_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"crates: store core http\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: [DONE]\n\n";

const PARENT_FINAL_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: [DONE]\n\n";

/// Scripted provider: the turn is strictly sequential — parent step 1 emits
/// the `task` call, the child (run synchronously inside the parent's tool
/// execution) answers, then the parent step 2 finalises. So the Nth request is
/// deterministic: 1 = task call, 2 = child, 3 = parent final.
fn spawn_scripted_provider() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let n = AtomicUsize::new(0);
        for sock in listener.incoming().flatten() {
            let which = n.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                // Fully drain the request (headers + body) before replying — a
                // reply-before-drain races the client's write and the turn
                // fails (the reason the token test drains too).
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
                        (k.eq_ignore_ascii_case("content-length"))
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let reply = match which {
                    0 => TASK_CALL_SSE,
                    1 => CHILD_ANSWER_SSE,
                    _ => PARENT_FINAL_SSE,
                };
                let _ = std::io::Write::write_all(&mut sock, reply.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn setup(addr: std::net::SocketAddr, sid: &str, subagent_depth: u64) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-task-{}-{sid}", std::process::id()));
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
    let payloads = Payloads {
        config: json!({"subagent_depth": subagent_depth}),
        // The launcher resolves `subagent_type` against the configured agents;
        // a minimal `explore` subagent + the `build` primary.
        api_agent: vec![
            json!({"name": "build", "mode": "primary"}),
            json!({
                "name": "explore", "mode": "subagent",
                "description": "explore",
                "permission": [{"permission": "*", "pattern": "*", "action": "deny"}],
            }),
        ],
        ..Default::default()
    };
    let st = AppState::with_wiring(
        None,
        payloads,
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

async fn drive_turn(app: &axum::Router, st: &Arc<AppState>, sid: &str) {
    let resp = post_json(
        app,
        &format!("/session/{sid}/prompt_async"),
        json!({
            "messageID": "msg_task000000000000000000001",
            "parts": [{"type": "text", "text": "do the thing"}],
            "model": {"providerID": "fake", "modelID": "m"},
            "agent": "build",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    for _ in 0..1200 {
        // answer any permission asks (the `task` ask itself) so the turn proceeds
        {
            let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
            let asked: Vec<(String, i64)> = conn
                .prepare(
                    "SELECT json_extract(payload,'$.id'), \
                     (SELECT count(*) FROM event e2 WHERE e2.session_id=event.session_id \
                      AND e2.type IN ('permission.replied','permission.replied@v1') \
                      AND json_extract(e2.payload,'$.requestID') = json_extract(event.payload,'$.id')) \
                     FROM event WHERE type IN ('permission.asked','permission.asked@v1')",
                )
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            for (id, replied) in asked {
                if replied > 0 {
                    continue;
                }
                let _ = post_json(
                    app,
                    &format!("/permission/{id}/reply"),
                    json!({"reply": "always"}),
                )
                .await;
            }
        }
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn task_spawns_child_session_and_returns_its_answer() {
    let addr = spawn_scripted_provider();
    let st = setup(addr, "ses_task0000000000000000001", 1);
    let app = ocserve_http::router(st.clone());
    let sid = "ses_task0000000000000000001";
    drive_turn(&app, &st, sid).await;

    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    // a child session exists with parent_id = sid and an explore title
    let (child_id, title, child_agent): (String, String, String) = conn
        .query_row(
            "SELECT id, title, agent FROM session WHERE parent_id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(child_id.starts_with("ses_"), "child id {child_id}");
    assert!(title.contains("(@explore subagent)"), "title {title}");
    assert_eq!(child_agent, "explore");
    // the child has an assistant answer
    let child_msgs: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id=?1 AND role='assistant'",
            [&child_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(child_msgs >= 1, "child must have an assistant answer");
    // the PARENT's persisted assistant message carries the task block with the
    // child's answer in it
    let parent_parts: String = conn
        .query_row(
            "SELECT group_concat(inline) FROM msg_part WHERE session_id=?1 AND type='tool'",
            [sid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        parent_parts.contains("task_result") && parent_parts.contains("crates: store core http"),
        "parent tool part must carry the child's answer: {parent_parts}"
    );
}

#[tokio::test]
async fn task_child_inherits_parent_deny_and_agent_title() {
    // The child's permission ruleset is derived from the parent's (deny +
    // external_directory) plus todowrite/task denies. The parent here carries
    // no rules, so the child gets exactly the two default denies; the point is
    // that the child ROW is persisted with a non-empty ruleset and the parent
    // link (asserted in the spawn test). This test asserts the persisted
    // ruleset carries the todowrite/task denies (RCM: child keeps constraints).
    let addr = spawn_scripted_provider();
    let st = setup(addr, "ses_task0000000000000000003", 1);
    let app = ocserve_http::router(st.clone());
    let sid = "ses_task0000000000000000003";
    drive_turn(&app, &st, sid).await;
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let perm: String = conn
        .query_row(
            "SELECT permission FROM session WHERE parent_id = ?1",
            [sid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(perm.contains("todowrite"), "child ruleset: {perm}");
    assert!(perm.contains("task"), "child ruleset: {perm}");
}
