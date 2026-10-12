//! v1 parity (prompt.ts:1093 + message-v2.ts:525-576): every model request
//! is built from filterCompacted history — after a completed compaction only
//! [anchor, summary] + the verbatim tail + everything after reach the model.
//!
//! The 2026-10-12 "session won't continue" bug: ocserve shipped the RAW
//! history to the provider, so a successful summarize relieved nothing — the
//! stuck session kept uploading ~6.5 MB per turn (a 5 MB tool part from the
//! previous day) at 90-190 s first-token and died at the watchdog. This test
//! captures the real provider request body and asserts the pre-tail history
//! (the 5 MB part) is NOT in it, the anchor renders as upstream's literal
//! "What did we do so far?", and the summary text IS in it.
//!
//! Negative control (run during development): replace the filter_compacted
//! call with the identity range -> the BIG marker re-appears in the body and
//! this test fails; restored -> green.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::io::Read as _;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SSE_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      cache-control: no-cache\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                      data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\
                      \"completion_tokens\":5,\"total_tokens\":15}}\n\n\
                      data: [DONE]\n\n";

/// Provider that records every request BODY it receives.
fn spawn_capture_provider() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let bodies2 = bodies.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let bodies = bodies2.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
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
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                bodies
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&got[head_end..head_end + clen]).to_string());
                let _ = std::io::Write::write_all(&mut sock, SSE_OK.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    (addr, bodies)
}

fn setup(addr: std::net::SocketAddr) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-ch-{}", std::process::id()));
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
            "id": "ses_ch", "projectID": "global", "directory": "/w",
            "path": "ses_ch", "slug": "ses_ch", "title": "ch", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

fn seed(st: &AppState, info: serde_json::Value, parts: Vec<serde_json::Value>) {
    ocserve_store::insert_message(&st.writer, Some(&*st.blobs), "ses_ch", &info, &parts).unwrap();
}

/// History graph: u1, a1(BIG), u_tail, a_tail, anchor(compaction part),
/// summary(summary=true), — exactly the post-compaction shape filter_compacted
/// reassembles: everything BEFORE u_tail is what the model must not see.
fn seed_compacted_history(st: &AppState) {
    seed(
        st,
        json!({"id":"msg_u1","sessionID":"ses_ch","role":"user","time":{"created":10}}),
        vec![json!({"id":"prt_u1","type":"text","text":"old question"})],
    );
    seed(
        st,
        json!({"id":"msg_a1","sessionID":"ses_ch","role":"assistant","time":{"created":11,"completed":12}}),
        vec![json!({"id":"prt_a1","type":"text","text": format!("BIGMARKER{}", "x".repeat(300_000))})],
    );
    seed(
        st,
        json!({"id":"msg_tail","sessionID":"ses_ch","role":"user","time":{"created":20}}),
        vec![json!({"id":"prt_tail","type":"text","text":"tail question"})],
    );
    seed(
        st,
        json!({"id":"msg_atail","sessionID":"ses_ch","role":"assistant","time":{"created":21,"completed":22}}),
        vec![json!({"id":"prt_atail","type":"text","text":"tail answer"})],
    );
    seed(
        st,
        json!({"id":"msg_anchor","sessionID":"ses_ch","role":"user","agent":"plan","time":{"created":30}}),
        vec![json!({"id":"prt_anchor","type":"compaction","auto":false,"overflow":false,
                    "tail_start_id":"msg_tail"})],
    );
    seed(
        st,
        json!({"id":"msg_summary","sessionID":"ses_ch","role":"assistant","summary":true,
               "parentID":"msg_anchor","finish":"stop","time":{"created":31,"completed":32}}),
        vec![json!({"id":"prt_summary","type":"text","text":"SUM-MARKER-OK: objective and details"})],
    );
}

#[tokio::test]
async fn provider_request_uses_filter_compacted_history() {
    let (addr, bodies) = spawn_capture_provider();
    let st = setup(addr);
    seed_compacted_history(&st);
    let app = ocserve_http::router(st.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_ch/message")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageID": "msg_newuser000000000000000001",
                        "parts": [{"type":"text","text":"new question"}],
                        "model": {"providerID":"fake","modelID":"m"},
                        "agent": "build"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bodies = bodies.lock().unwrap().clone();
    assert!(!bodies.is_empty(), "provider must have been called");
    let body = bodies.last().unwrap();
    assert!(
        body.len() < 100_000,
        "compacted request must be small, got {} bytes",
        body.len()
    );
    assert!(
        body.contains("What did we do so far?"),
        "anchor must render upstream's literal compaction question"
    );
    assert!(
        body.contains("SUM-MARKER-OK"),
        "the summary text must reach the model"
    );
    assert!(body.contains("tail question"), "verbatim tail kept");
    assert!(body.contains("new question"), "new user message kept");
    assert!(
        !body.contains("BIGMARKER"),
        "pre-tail history must NOT reach the model (the 6.5 MB-per-turn bug)"
    );
}
