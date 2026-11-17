//! Issue #46: interrupt must settle a turn blocked on a pending ask instead
//! of leaving the ask (and the client card) alive until its timeout.
//!
//! The turn asks for an external directory; we interrupt while the ask is
//! pending and require: a fast response, `permission.replied: reject`, the
//! v2 `session.execution.interrupted` event, and an empty pending list.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn read_tool_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_ia_1\",\"type\":\"function\",\"function\":{\
     \"name\":\"read\",\"arguments\":\"{\\\"filePath\\\":\\\"/etc/hostname\\\"}\"\
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

fn spawn_provider(n: Arc<AtomicUsize>) -> std::net::SocketAddr {
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
                let body = if seq == 0 {
                    read_tool_sse()
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
    let dir = std::env::temp_dir().join(format!("ocserve-ia-{}", std::process::id()));
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
        modalities: Default::default(),
        limits,
        endpoints: [("fake".into(), (format!("http://{addr}"), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    let st = AppState::with_wiring_and_v2(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
        true,
    );
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": "ses_ia", "projectID": "global", "directory": "/w",
            "path": "ses_ia", "slug": "ses_ia", "title": "ia", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

fn events(st: &AppState, ty: &str) -> Vec<Value> {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let mut stmt = conn
        .prepare("SELECT type, payload FROM event WHERE session_id='ses_ia' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|(t, _)| t == ty || t == &format!("{ty}@v1"))
        .filter_map(|(_, p)| serde_json::from_str::<Value>(&p).ok())
        .collect()
}

async fn wait_for(st: &AppState, ty: &str) -> Value {
    for _ in 0..1000 {
        if let Some(v) = events(st, ty).into_iter().next() {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{ty} never emitted");
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn interrupt_settles_a_pending_permission_ask() {
    let addr = spawn_provider(Arc::new(AtomicUsize::new(0)));
    let st = setup(addr);
    let app = ocserve_http::router(st.clone());

    let (status, _) = post(
        &app,
        "/api/session/ses_ia/prompt",
        json!({"text": "read /etc/hostname"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let asked = wait_for(&st, "permission.asked").await;
    let rid = asked["id"].as_str().expect("request id").to_string();

    let started = std::time::Instant::now();
    let (status, body) = post(&app, "/api/session/ses_ia/interrupt", json!({})).await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["interrupted"], true);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "interrupt must not wait for the ask timeout, took {elapsed:?}"
    );

    // The turn ends as interrupted, not after a 300 s ask.
    wait_for(&st, "session.execution.interrupted").await;
    let replied = wait_for(&st, "permission.replied").await;
    assert_eq!(replied["requestID"], rid.as_str());
    assert_eq!(replied["reply"], "reject");

    // The card source is gone and the claim released.
    assert!(
        st.gate.pending_ids_for_session("ses_ia").is_empty(),
        "pending permission survives interrupt"
    );
    assert!(!ocserve_store::claim::held(&st.db, "ses_ia").unwrap());
    assert!(
        events(&st, "session.execution.succeeded").is_empty(),
        "an interrupted turn must not also report success"
    );
}
