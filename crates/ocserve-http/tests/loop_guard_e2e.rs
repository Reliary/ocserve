//! D1/P1b e2e: three identical bash tool-calls → upstream-parity
//! `permission.asked` with `action="doom_loop"` (metadata.class=repeat),
//! approval executes the third call, prompt completes. Metrics asserted.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn tool_call_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_lg_1\",\"type\":\"function\",\"function\":{\
     \"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"echo LOOPTEST\\\"}\"\
     }}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
}

fn final_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\
     \"completion_tokens\":5,\"total_tokens\":105}}\n\n\
     data: [DONE]\n\n"
}

/// Requests 1..3 get the identical tool call (3rd must trip the guard);
/// request 4+ finishes with text.
fn spawn_loop_provider(n: Arc<AtomicUsize>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let n = n.clone();
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
                        if k.eq_ignore_ascii_case("content-length") {
                            v.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let seq = n.fetch_add(1, Ordering::SeqCst);
                let body = if seq < 3 {
                    tool_call_sse()
                } else {
                    final_sse()
                };
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

fn setup(addr: std::net::SocketAddr) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-lg-{}", std::process::id()));
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
            "id": "ses_lg", "projectID": "global", "directory": "/w",
            "path": "ses_lg", "slug": "ses_lg", "title": "lg", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

#[tokio::test]
async fn third_identical_call_asks_doom_loop_then_completes_on_approve() {
    let n = Arc::new(AtomicUsize::new(0));
    let addr = spawn_loop_provider(n.clone());
    let st = setup(addr);
    let app = ocserve_http::router(st.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_lg/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageId": "msg_lg0000000000000000000001",
                        "parts": [{"type": "text", "text": "repeat yourself"}],
                        "model": {"providerID": "fake", "modelID": "m"},
                        "agent": "build",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Answer every permission ask until doom_loop appears: the first bash
    // call trips the NORMAL rule ask (default deny->ask) — reply `always`
    // so the rule grants and calls #1-2 run unasked; call #3 must be doom.
    let mut doom_id: Option<String> = None;
    for _ in 0..400 {
        let (asked, replied): (Vec<String>, Vec<String>) = {
            let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
            let asked = conn
                .prepare(
                    "SELECT payload FROM event WHERE session_id='ses_lg' AND type='permission.asked'",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            let replied = conn
                .prepare(
                    "SELECT json_extract(payload,'$.requestID') FROM event WHERE session_id='ses_lg' AND type='permission.replied'",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            (asked, replied)
        };
        for data in &asked {
            let v: serde_json::Value = serde_json::from_str(data).unwrap();
            let id = v["id"].as_str().unwrap().to_string();
            if replied.contains(&id) {
                continue;
            }
            let (perm_id, reply) = if v["permission"] == "doom_loop" {
                assert_eq!(v["metadata"]["class"], "repeat", "payload={data}");
                assert_eq!(v["patterns"], json!(["bash"]));
                assert!(doom_id.is_none(), "exactly one doom ask");
                doom_id = Some(id.clone());
                (id, "once")
            } else {
                (id, "always")
            };
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/permission/{perm_id}/reply"))
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"reply": reply}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        if doom_id.is_some() {
            let tasks_done = st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty();
            if tasks_done {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        doom_id.is_some(),
        "doom_loop permission.asked never emitted (guard did not fire)"
    );

    for _ in 0..800 {
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty(),
        "prompt did not complete after approval"
    );

    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    // three tool parts: 1st, 2nd (pre-fire) and 3rd (post-approval) all ran
    let tools: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_lg' AND type='tool'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tools, 3, "approved 3rd call must have executed");
    let inline: String = conn
        .query_row(
            "SELECT p.inline FROM msg_part p \
             JOIN msg m ON m.id=p.message_id \
             WHERE m.role='assistant' AND p.type='text' AND p.inline IS NOT NULL LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let part: serde_json::Value = serde_json::from_str(&inline).unwrap();
    assert_eq!(part["text"], "done", "final answer persisted");

    // detection metric surfaced on /metrics
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let exposition = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        exposition
            .lines()
            .any(|l| l.starts_with("ocserve_agent_health_total{class=\"repeat\"}")),
        "repeat metric missing:\n{exposition}"
    );
}
