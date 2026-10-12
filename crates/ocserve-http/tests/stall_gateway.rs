//! 2026-10-12 gateway-hang regression: a provider that answers with headers
//! and then streams ONLY `: keep-alive` comment frames must be treated as
//! "no data yet" — the first real event gets the longer TTFB budget, and a
//! keep-alive-only stream must NOT satisfy the watchdog (the pre-fix code
//! counted any socket byte as liveness, which is fine, but it applied the
//! 120 s stream budget to the model-wait phase; the live failure: the
//! opencode-go gateway held the free-tier model for >120 s while pinging
//! comments, killing the user's turn four times).
//!
//! Two assertions, one per phase, both on their own process (env is
//! process-local by construction: this test binary runs a single test):
//!  1. keep-alive-only forever → the turn must FAIL within the TTFB budget
//!     (not hang, not be mistaken for progress).
//!  2. headers + keep-alives + real data, with the FIRST data deliberately
//!     later than the stream budget but within the TTFB budget → the turn
//!     must SUCCEED (this is the user's exact scenario).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

fn app_with(addr: std::net::SocketAddr) -> std::sync::Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-tg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [(
            "fake".into(),
            (format!("http://{addr}"), "test-key".to_string()),
        )]
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
        &serde_json::json!({
            "id": "ses_tg", "projectID": "global", "directory": "/w", "path": "ses_tg",
            "slug": "ses_tg", "title": "tg", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

/// Minimal SSE server: sends keep-alive comments every 200 ms; if `first_data_after`
/// is Some(d), sends a real text delta + `[DONE]` after d.
fn spawn_sse_server(first_data_after: Option<std::time::Duration>) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            std::thread::spawn(move || {
                // read request head + body (best-effort; Content-Length aware)
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
                );
                let _ = stream.write_all(b": keep-alive\n\n");
                let t0 = std::time::Instant::now();
                match first_data_after {
                    Some(d) => {
                        while t0.elapsed() < d {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            let _ = stream.write_all(b": keep-alive\n\n");
                        }
                        let chunk = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":null}]}\n\n";
                        let _ = stream.write_all(chunk.as_bytes());
                        let _ = stream.write_all(
                            b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"total_tokens\":11}}\n\n",
                        );
                        let _ = stream.write_all(b"data: [DONE]\n\n");
                    }
                    None => loop {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        if stream.write_all(b": keep-alive\n\n").is_err() {
                            break;
                        }
                    },
                }
            });
        }
    });
    addr
}

async fn prompt(st: &std::sync::Arc<AppState>) -> StatusCode {
    ocserve_http::router(st.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_tg/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"messageID":"msg_tg0000000000000000000001",
                        "parts":[{"type":"text","text":"hi"}],
                        "model":{"providerID":"fake","modelID":"m"},"agent":"build"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}



async fn wait_idle(st: &std::sync::Arc<AppState>, budget: std::time::Duration) -> bool {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < budget {
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn keepalive_only_forever_cannot_pass_as_progress() {
    // SAFETY: single #[tokio::test] in this binary (own process env).
    unsafe { std::env::set_var("OCSERVE_PROVIDER_TTFB_SECS", "2") };
    unsafe { std::env::set_var("OCSERVE_PROVIDER_STALL_SECS", "30") };
    let addr = spawn_sse_server(None);
    let st = app_with(addr);
    assert_eq!(prompt(&st).await, StatusCode::NO_CONTENT);
    let t0 = std::time::Instant::now();
    assert!(
        wait_idle(&st, std::time::Duration::from_secs(20)).await,
        "keep-alive-only stream must trip the TTFB watchdog and release state"
    );
    let el = t0.elapsed();
    assert!(
        el >= std::time::Duration::from_secs(2) && el < std::time::Duration::from_secs(15),
        "should trip at the TTFB budget (~2s), took {el:?}"
    );
}
